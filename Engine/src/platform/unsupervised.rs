//! Where nothing supervises a confined command's connections (macOS: Seatbelt
//! decides each one by itself; Windows: the restricted token opens no other
//! program's pipe), a live upgrade has no supervisor to hand on — said by a
//! value (`None`), never by one that pretends.

use std::io;

use super::{Handed, SupervisedAs};

/// No supervisor exists here, so none is ever let go of.
pub enum Supervision {}

impl Supervision {
    pub fn resume(self) {
        match self {}
    }

    pub fn into_handed(self) -> io::Result<Handed> {
        match self {}
    }
}

pub fn release_supervision(
    _root: i32,
    _within: std::time::Duration,
) -> io::Result<Option<Supervision>> {
    Ok(None)
}

pub fn adopt_supervision(
    _handed: Handed,
    _root: i32,
    _tree: Option<&str>,
    _answers: SupervisedAs<'_>,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no connection supervisor exists on this system",
    ))
}
