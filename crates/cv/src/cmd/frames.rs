//! Header holdback shared by transcript and HTML renderers. A frame is already rendered;
//! readers remain responsible for materialization, redaction and byte budgets.
use std::io::Write;

pub(super) struct Frames<'a, W> {
    pub out: &'a mut W,
    pub buf: Vec<String>,
    pub printed: bool,
    pub result: std::io::Result<()>,
}
impl<'a, W: Write> Frames<'a, W> {
    pub fn new(out: &'a mut W) -> Self {
        Self {
            out,
            buf: Vec::new(),
            printed: false,
            result: Ok(()),
        }
    }
    pub fn ready(model: bool, title: bool, messages: usize) -> bool {
        (model && title) || messages >= 24
    }
    pub fn write(&mut self, text: &str) {
        if self.result.is_ok() {
            self.result = self.out.write_all(text.as_bytes());
        }
    }
    pub fn push(&mut self, text: String) {
        if self.printed {
            self.write(&text);
        } else {
            self.buf.push(text);
        }
    }
    pub fn start(&mut self, header: &str) {
        if self.printed {
            return;
        }
        self.write(header);
        for frame in std::mem::take(&mut self.buf) {
            self.write(&frame);
        }
        self.printed = true;
    }
    pub fn take_error(&mut self) -> Option<anyhow::Error> {
        if self.result.is_err() {
            std::mem::replace(
                &mut self.result,
                Err(std::io::Error::other("render output has already failed")),
            )
            .err()
            .map(Into::into)
        } else {
            None
        }
    }
}
