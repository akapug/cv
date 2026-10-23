//! The pretty Session envelopes around staged message frames: the archive's (with its reusable
//! stable prefix) and the CLI's (optionally as an OpenSession document). The writer itself is
//! [`cv_core::projection::write_frames`].
use anyhow::Result;
use cv_core::bytes::ByteSource;
use cv_core::projection::{write_frames, Framing, Prefix};
use cv_core::Session;
use std::{io::Write, path::Path};

pub(crate) fn write_session(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    let framing = Framing {
        pretty: true,
        old,
        stable,
        total: Some(total),
    };
    write_frames(writer, session, "messages", spool, framing)
}

pub(crate) fn write_document(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    total: usize,
    document: bool,
) -> Result<()> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    let framing = Framing {
        pretty: true,
        total: Some(total),
        ..Framing::default()
    };
    if document {
        write_frames(
            writer,
            &cv_core::harness::opensession::document(session),
            "messages",
            spool,
            framing,
        )?;
    } else {
        write_frames(writer, session, "messages", spool, framing)?;
    }
    Ok(())
}
