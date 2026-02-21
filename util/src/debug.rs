#[derive(PartialEq)]
pub struct AsciiDebug<'s>(pub &'s [u8]);

impl<'s> std::fmt::Debug for AsciiDebug<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"")?;
        for b in self.0 {
            for c in std::ascii::escape_default(*b) {
                write!(f, "{}", c as char)?;
            }
        }
        write!(f, "\"")?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::fmt::Write;

    use super::AsciiDebug;

    #[test]
    fn ascii() {
        let mut s = String::new();
        let i = [b'a', b'b', 0, b'c'];
        write!(&mut s, "{:?}", AsciiDebug(&i)).unwrap();
        assert_eq!("\"ab\\x00c\"", &s);
    }
}
