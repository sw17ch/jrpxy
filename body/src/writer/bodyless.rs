#[derive(Debug)]
pub struct BodylessBodyWriter<I>(I);

impl<I> BodylessBodyWriter<I> {
    pub fn new(writer: I) -> Self {
        Self(writer)
    }

    pub fn finish(self) -> I {
        let Self(writer) = self;
        writer
    }
}
