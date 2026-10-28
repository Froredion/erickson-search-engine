//! Porter stemmer (M.F. Porter, 1980): strips English suffixes so "connected", "connecting"
//! and "connection" all index as "connect". Operates on lowercase ASCII words only.

/// Stems `word` in place. Words that aren't lowercase ASCII letters, or are 2 bytes or shorter,
/// are left alone.
pub fn stem(word: &mut String) {
    if word.len() <= 2 || !word.bytes().all(|c| c.is_ascii_lowercase()) {
        return;
    }
    let b = std::mem::take(word).into_bytes();
    let k = b.len() as isize - 1;
    let mut p = Porter { b, k, j: 0 };
    p.step1ab();
    if p.k > 0 {
        p.step1c();
        p.step2();
        p.step3();
        p.step4();
        p.step5();
    }
    p.b.truncate((p.k + 1) as usize);
    *word = String::from_utf8(p.b).expect("ASCII in, ASCII out");
}

/// `b[0..=k]` is the word being stemmed; `j` marks the end of the stem before a matched suffix.
struct Porter {
    b: Vec<u8>,
    k: isize,
    j: isize,
}

impl Porter {
    fn at(&self, i: isize) -> u8 {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => i == 0 || !self.cons(i - 1),
            _ => true,
        }
    }

    /// Number of vowel-consonant sequences in `b[0..=j]`.
    fn m(&self) -> usize {
        let (mut n, mut i) = (0, 0);
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    fn double_cons(&self, i: isize) -> bool {
        i >= 1 && self.at(i) == self.at(i - 1) && self.cons(i)
    }

    /// consonant-vowel-consonant ending at `i`, where the last consonant isn't w, x or y.
    fn cvc(&self, i: isize) -> bool {
        i >= 2 && self.cons(i) && !self.cons(i - 1) && self.cons(i - 2) && !matches!(self.at(i), b'w' | b'x' | b'y')
    }

    fn ends(&mut self, s: &str) -> bool {
        let len = s.len() as isize;
        if len > self.k + 1 || &self.b[(self.k - len + 1) as usize..=self.k as usize] != s.as_bytes() {
            return false;
        }
        self.j = self.k - len;
        true
    }

    fn set_to(&mut self, s: &str) {
        self.b.truncate((self.j + 1) as usize);
        self.b.extend_from_slice(s.as_bytes());
        self.k = self.j + s.len() as isize;
    }

    fn set_k(&mut self, k: isize) {
        self.k = k;
        self.b.truncate((k + 1) as usize);
    }

    fn replace_if_m(&mut self, s: &str) {
        if self.m() > 0 {
            self.set_to(s);
        }
    }

    /// Plurals and -ed / -ing.
    fn step1ab(&mut self) {
        if self.at(self.k) == b's' {
            if self.ends("sses") {
                self.set_k(self.k - 2);
            } else if self.ends("ies") {
                self.set_to("i");
            } else if self.at(self.k - 1) != b's' {
                self.set_k(self.k - 1);
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.set_k(self.k - 1);
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.set_k(self.j);
            if self.ends("at") {
                self.set_to("ate");
            } else if self.ends("bl") {
                self.set_to("ble");
            } else if self.ends("iz") {
                self.set_to("ize");
            } else if self.double_cons(self.k) {
                if !matches!(self.at(self.k), b'l' | b's' | b'z') {
                    self.set_k(self.k - 1);
                }
            } else {
                self.j = self.k;
                if self.m() == 1 && self.cvc(self.k) {
                    self.set_to("e");
                }
            }
        }
    }

    /// Terminal y to i when there's another vowel in the stem.
    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            self.b[self.k as usize] = b'i';
        }
    }

    /// Double suffixes to single ones: -ization -> -ize, -ational -> -ate, ...
    fn step2(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k - 1) {
            b'a' => &[("ational", "ate"), ("tional", "tion")],
            b'c' => &[("enci", "ence"), ("anci", "ance")],
            b'e' => &[("izer", "ize")],
            b'l' => &[("bli", "ble"), ("alli", "al"), ("entli", "ent"), ("eli", "e"), ("ousli", "ous")],
            b'o' => &[("ization", "ize"), ("ation", "ate"), ("ator", "ate")],
            b's' => &[("alism", "al"), ("iveness", "ive"), ("fulness", "ful"), ("ousness", "ous")],
            b't' => &[("aliti", "al"), ("iviti", "ive"), ("biliti", "ble")],
            b'g' => &[("logi", "log")],
            _ => &[],
        };
        for (from, to) in rules {
            if self.ends(from) {
                self.replace_if_m(to);
                return;
            }
        }
    }

    /// -ic-, -full, -ness, ...
    fn step3(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k) {
            b'e' => &[("icate", "ic"), ("ative", ""), ("alize", "al")],
            b'i' => &[("iciti", "ic")],
            b'l' => &[("ical", "ic"), ("ful", "")],
            b's' => &[("ness", "")],
            _ => &[],
        };
        for (from, to) in rules {
            if self.ends(from) {
                self.replace_if_m(to);
                return;
            }
        }
    }

    /// -ant, -ence, ... when the remaining stem is long enough (m > 1).
    fn step4(&mut self) {
        let suffixes: &[&str] = match self.at(self.k - 1) {
            b'a' => &["al"],
            b'c' => &["ance", "ence"],
            b'e' => &["er"],
            b'i' => &["ic"],
            b'l' => &["able", "ible"],
            b'n' => &["ant", "ement", "ment", "ent"],
            b'o' => &["ion", "ou"],
            b's' => &["ism"],
            b't' => &["ate", "iti"],
            b'u' => &["ous"],
            b'v' => &["ive"],
            b'z' => &["ize"],
            _ => return,
        };
        for s in suffixes {
            if self.ends(s) {
                if *s == "ion" && !(self.j >= 0 && matches!(self.at(self.j), b's' | b't')) {
                    return;
                }
                if self.m() > 1 {
                    self.set_k(self.j);
                }
                return;
            }
        }
    }

    /// Final -e, and -ll to -l.
    fn step5(&mut self) {
        self.j = self.k;
        if self.at(self.k) == b'e' {
            let m = self.m();
            if m > 1 || (m == 1 && !self.cvc(self.k - 1)) {
                self.set_k(self.k - 1);
            }
        }
        self.j = self.k;
        if self.at(self.k) == b'l' && self.double_cons(self.k) && self.m() > 1 {
            self.set_k(self.k - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn porter_reference_words() {
        let cases = [
            ("caresses", "caress"), ("ponies", "poni"), ("ties", "ti"), ("cats", "cat"),
            ("feed", "feed"), ("agreed", "agre"), ("plastered", "plaster"), ("motoring", "motor"),
            ("sing", "sing"), ("conflated", "conflat"), ("troubled", "troubl"), ("sized", "size"),
            ("hopping", "hop"), ("falling", "fall"), ("hissing", "hiss"), ("filing", "file"),
            ("happy", "happi"), ("sky", "sky"), ("relational", "relat"), ("conditional", "condit"),
            ("rational", "ration"), ("digitizer", "digit"), ("operator", "oper"),
            ("generalization", "gener"), ("hopefulness", "hope"), ("electrical", "electr"),
            ("goodness", "good"), ("revival", "reviv"), ("allowance", "allow"), ("adjustment", "adjust"),
            ("adoption", "adopt"), ("connection", "connect"), ("connected", "connect"),
            ("connecting", "connect"), ("running", "run"), ("vaccines", "vaccin"), ("controll", "control"),
            ("roll", "roll"), ("probate", "probat"), ("rate", "rate"), ("cease", "ceas"),
        ];
        for (word, want) in cases {
            let mut w = word.to_owned();
            stem(&mut w);
            assert_eq!(w, want, "stem({word})");
        }
    }
}
