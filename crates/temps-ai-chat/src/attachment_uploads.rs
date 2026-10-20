// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Chunked attachment uploads.
//!
//! A file arrives as a start request, a run of small multipart chunks, and a
//! completion. Each request stays far below the body size a reverse proxy in
//! front of the console may allow (a Fleet preview rejects anything over 40 KiB),
//! and the file is only stored, through the same path as a single-request
//! upload, once every byte has arrived.
//!
//! Memory is bounded: a session reserves its declared size up front against a
//! process-wide budget, a user may hold only a few sessions open, and a
//! session nobody finishes is dropped after [`SESSION_TTL`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use thiserror::Error;

/// How much of a file one chunk request carries. Leaves room for the multipart
/// framing under a 40 KiB proxy limit.
pub const UPLOAD_CHUNK_BYTES: usize = 32 * 1024;
/// Uploads one user may have in flight at once (a message takes four files).
pub const MAX_ACTIVE_UPLOADS_PER_USER: usize = 8;
/// Everything all in-flight uploads may reserve together.
pub const MAX_RESERVED_BYTES: usize = 64 * 1024 * 1024;
/// An upload that has not finished by then is discarded.
pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UploadError {
    #[error("Attachment upload '{upload_id}' not found for conversation '{conversation_id}' (it may have expired after {ttl_secs}s)")]
    NotFound {
        upload_id: String,
        conversation_id: String,
        ttl_secs: u64,
    },
    #[error("Attachment '{name}' is empty; there is nothing to upload")]
    Empty { name: String },
    #[error("Attachment '{name}' is {declared} bytes, over the {limit}-byte limit")]
    TooLarge {
        name: String,
        declared: usize,
        limit: usize,
    },
    #[error("User {user_id} already has {active} uploads in progress (limit {limit}); finish or cancel one first")]
    TooManyActive {
        user_id: i32,
        active: usize,
        limit: usize,
    },
    #[error("Uploads in progress already hold {reserved} bytes; the server allows {limit} at once, try again shortly")]
    OutOfCapacity { reserved: usize, limit: usize },
    #[error(
        "Upload '{upload_id}' expected the next chunk at offset {expected} but got offset {got}"
    )]
    OutOfOrder {
        upload_id: String,
        expected: usize,
        got: usize,
    },
    #[error("Upload '{upload_id}' chunk of {chunk} bytes exceeds the {limit}-byte chunk size")]
    ChunkTooLarge {
        upload_id: String,
        chunk: usize,
        limit: usize,
    },
    #[error("Upload '{upload_id}' chunk would take it to {would_be} bytes, past the {declared} bytes declared at the start")]
    PastDeclaredSize {
        upload_id: String,
        would_be: usize,
        declared: usize,
    },
    #[error("Upload '{upload_id}' received {received} of {declared} bytes; send the rest before completing")]
    Incomplete {
        upload_id: String,
        received: usize,
        declared: usize,
    },
}

struct Session {
    user_id: i32,
    conversation_id: String,
    name: String,
    declared_mime: Option<String>,
    declared_size: usize,
    buffer: Vec<u8>,
    touched: Instant,
}

/// What a finished upload hands to storage.
#[derive(Debug)]
pub struct FinishedUpload {
    pub name: String,
    pub declared_mime: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
pub struct UploadSessions {
    sessions: Mutex<HashMap<String, Session>>,
}

impl UploadSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open an upload for a file of `size` bytes. `limit` is the per-file cap.
    pub fn start(
        &self,
        user_id: i32,
        conversation_id: &str,
        name: &str,
        declared_mime: Option<String>,
        size: usize,
        limit: usize,
    ) -> Result<String, UploadError> {
        self.start_at(
            Instant::now(),
            user_id,
            conversation_id,
            name,
            declared_mime,
            size,
            limit,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_at(
        &self,
        now: Instant,
        user_id: i32,
        conversation_id: &str,
        name: &str,
        declared_mime: Option<String>,
        size: usize,
        limit: usize,
    ) -> Result<String, UploadError> {
        if size == 0 {
            return Err(UploadError::Empty {
                name: name.to_string(),
            });
        }
        if size > limit {
            return Err(UploadError::TooLarge {
                name: name.to_string(),
                declared: size,
                limit,
            });
        }
        let mut sessions = self.lock();
        sessions.retain(|_, session| now.duration_since(session.touched) < SESSION_TTL);
        let active = sessions
            .values()
            .filter(|session| session.user_id == user_id)
            .count();
        if active >= MAX_ACTIVE_UPLOADS_PER_USER {
            return Err(UploadError::TooManyActive {
                user_id,
                active,
                limit: MAX_ACTIVE_UPLOADS_PER_USER,
            });
        }
        let reserved: usize = sessions.values().map(|session| session.declared_size).sum();
        if reserved.saturating_add(size) > MAX_RESERVED_BYTES {
            return Err(UploadError::OutOfCapacity {
                reserved,
                limit: MAX_RESERVED_BYTES,
            });
        }
        let upload_id = format!("upl_{}", uuid::Uuid::new_v4().simple());
        sessions.insert(
            upload_id.clone(),
            Session {
                user_id,
                conversation_id: conversation_id.to_string(),
                name: name.to_string(),
                declared_mime,
                declared_size: size,
                buffer: Vec::new(),
                touched: now,
            },
        );
        Ok(upload_id)
    }

    /// Add the chunk that starts at `offset`; returns the bytes held so far.
    /// Sending the chunk just applied again (a retry after a lost response) is
    /// accepted and changes nothing.
    pub fn append(
        &self,
        user_id: i32,
        conversation_id: &str,
        upload_id: &str,
        offset: usize,
        chunk: &[u8],
    ) -> Result<usize, UploadError> {
        self.append_at(
            Instant::now(),
            user_id,
            conversation_id,
            upload_id,
            offset,
            chunk,
        )
    }

    fn append_at(
        &self,
        now: Instant,
        user_id: i32,
        conversation_id: &str,
        upload_id: &str,
        offset: usize,
        chunk: &[u8],
    ) -> Result<usize, UploadError> {
        let mut sessions = self.lock();
        let session = Self::session_mut(&mut sessions, now, user_id, conversation_id, upload_id)?;
        if chunk.len() > UPLOAD_CHUNK_BYTES {
            return Err(UploadError::ChunkTooLarge {
                upload_id: upload_id.to_string(),
                chunk: chunk.len(),
                limit: UPLOAD_CHUNK_BYTES,
            });
        }
        let received = session.buffer.len();
        let end = offset.saturating_add(chunk.len());
        if end == received && offset < received && session.buffer[offset..] == *chunk {
            session.touched = now;
            return Ok(received);
        }
        if offset != received {
            return Err(UploadError::OutOfOrder {
                upload_id: upload_id.to_string(),
                expected: received,
                got: offset,
            });
        }
        if end > session.declared_size {
            return Err(UploadError::PastDeclaredSize {
                upload_id: upload_id.to_string(),
                would_be: end,
                declared: session.declared_size,
            });
        }
        session.buffer.extend_from_slice(chunk);
        session.touched = now;
        Ok(session.buffer.len())
    }

    /// Take the finished file out. The upload is gone afterwards either way a
    /// caller handles the result: a completed file is stored once, not twice.
    pub fn finish(
        &self,
        user_id: i32,
        conversation_id: &str,
        upload_id: &str,
    ) -> Result<FinishedUpload, UploadError> {
        self.finish_at(Instant::now(), user_id, conversation_id, upload_id)
    }

    fn finish_at(
        &self,
        now: Instant,
        user_id: i32,
        conversation_id: &str,
        upload_id: &str,
    ) -> Result<FinishedUpload, UploadError> {
        let mut sessions = self.lock();
        let session = Self::session_mut(&mut sessions, now, user_id, conversation_id, upload_id)?;
        if session.buffer.len() != session.declared_size {
            return Err(UploadError::Incomplete {
                upload_id: upload_id.to_string(),
                received: session.buffer.len(),
                declared: session.declared_size,
            });
        }
        let Some(session) = sessions.remove(upload_id) else {
            return Err(Self::not_found(upload_id, conversation_id));
        };
        Ok(FinishedUpload {
            name: session.name,
            declared_mime: session.declared_mime,
            bytes: session.buffer,
        })
    }

    /// Drop an upload and free what it reserved. Unknown ids are not an error:
    /// the goal (no upload left behind) already holds.
    pub fn cancel(&self, user_id: i32, conversation_id: &str, upload_id: &str) {
        let mut sessions = self.lock();
        let owned = sessions
            .get(upload_id)
            .is_some_and(|s| s.user_id == user_id && s.conversation_id == conversation_id);
        if owned {
            sessions.remove(upload_id);
        }
    }

    fn session_mut<'a>(
        sessions: &'a mut HashMap<String, Session>,
        now: Instant,
        user_id: i32,
        conversation_id: &str,
        upload_id: &str,
    ) -> Result<&'a mut Session, UploadError> {
        let expired = sessions
            .get(upload_id)
            .is_some_and(|s| now.duration_since(s.touched) >= SESSION_TTL);
        if expired {
            sessions.remove(upload_id);
        }
        match sessions.get_mut(upload_id) {
            Some(session)
                if session.user_id == user_id && session.conversation_id == conversation_id =>
            {
                Ok(session)
            }
            _ => Err(Self::not_found(upload_id, conversation_id)),
        }
    }

    fn not_found(upload_id: &str, conversation_id: &str) -> UploadError {
        UploadError::NotFound {
            upload_id: upload_id.to_string(),
            conversation_id: conversation_id.to_string(),
            ttl_secs: SESSION_TTL.as_secs(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        // A panic while holding the lock cannot leave the map half-edited in a
        // way later requests cannot cope with, so keep serving.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 1024 * 1024;

    fn start(sessions: &UploadSessions, user: i32, size: usize) -> String {
        sessions
            .start(
                user,
                "conv",
                "shot.png",
                Some("image/png".into()),
                size,
                LIMIT,
            )
            .expect("start")
    }

    #[test]
    fn a_file_sent_in_chunks_comes_back_whole() {
        let sessions = UploadSessions::new();
        let data: Vec<u8> = (0..100_000u32).map(|n| (n % 251) as u8).collect();
        let id = start(&sessions, 1, data.len());
        let mut offset = 0;
        for chunk in data.chunks(UPLOAD_CHUNK_BYTES) {
            offset = sessions.append(1, "conv", &id, offset, chunk).unwrap();
        }
        assert_eq!(offset, data.len());
        let done = sessions.finish(1, "conv", &id).unwrap();
        assert_eq!(done.bytes, data);
        assert_eq!(done.name, "shot.png");
        assert_eq!(done.declared_mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn finishing_twice_stores_once() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 3);
        sessions.append(1, "conv", &id, 0, b"abc").unwrap();
        sessions.finish(1, "conv", &id).unwrap();
        assert!(matches!(
            sessions.finish(1, "conv", &id),
            Err(UploadError::NotFound { .. })
        ));
    }

    #[test]
    fn an_incomplete_file_cannot_be_finished_but_can_be_continued() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 6);
        sessions.append(1, "conv", &id, 0, b"abc").unwrap();
        assert_eq!(
            sessions.finish(1, "conv", &id).unwrap_err(),
            UploadError::Incomplete {
                upload_id: id.clone(),
                received: 3,
                declared: 6
            }
        );
        sessions.append(1, "conv", &id, 3, b"def").unwrap();
        assert_eq!(sessions.finish(1, "conv", &id).unwrap().bytes, b"abcdef");
    }

    #[test]
    fn a_gap_or_a_rewind_with_different_bytes_is_refused() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 9);
        sessions.append(1, "conv", &id, 0, b"abc").unwrap();
        assert!(matches!(
            sessions.append(1, "conv", &id, 5, b"xyz"),
            Err(UploadError::OutOfOrder {
                expected: 3,
                got: 5,
                ..
            })
        ));
        assert!(matches!(
            sessions.append(1, "conv", &id, 0, b"zzz"),
            Err(UploadError::OutOfOrder {
                expected: 3,
                got: 0,
                ..
            })
        ));
    }

    #[test]
    fn resending_the_last_chunk_after_a_lost_response_is_harmless() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 6);
        sessions.append(1, "conv", &id, 0, b"abc").unwrap();
        assert_eq!(sessions.append(1, "conv", &id, 0, b"abc").unwrap(), 3);
        sessions.append(1, "conv", &id, 3, b"def").unwrap();
        assert_eq!(sessions.finish(1, "conv", &id).unwrap().bytes, b"abcdef");
    }

    #[test]
    fn more_bytes_than_declared_are_refused() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 4);
        assert!(matches!(
            sessions.append(1, "conv", &id, 0, b"abcde"),
            Err(UploadError::PastDeclaredSize {
                would_be: 5,
                declared: 4,
                ..
            })
        ));
    }

    #[test]
    fn a_chunk_over_the_chunk_size_is_refused() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, UPLOAD_CHUNK_BYTES * 2);
        let big = vec![0u8; UPLOAD_CHUNK_BYTES + 1];
        assert!(matches!(
            sessions.append(1, "conv", &id, 0, &big),
            Err(UploadError::ChunkTooLarge { .. })
        ));
    }

    #[test]
    fn empty_and_oversized_files_are_refused_at_the_start() {
        let sessions = UploadSessions::new();
        assert!(matches!(
            sessions.start(1, "conv", "a", None, 0, LIMIT),
            Err(UploadError::Empty { .. })
        ));
        assert!(matches!(
            sessions.start(1, "conv", "a", None, LIMIT + 1, LIMIT),
            Err(UploadError::TooLarge { .. })
        ));
    }

    #[test]
    fn another_user_or_chat_cannot_touch_an_upload() {
        let sessions = UploadSessions::new();
        let id = start(&sessions, 1, 3);
        assert!(matches!(
            sessions.append(2, "conv", &id, 0, b"abc"),
            Err(UploadError::NotFound { .. })
        ));
        assert!(matches!(
            sessions.append(1, "other", &id, 0, b"abc"),
            Err(UploadError::NotFound { .. })
        ));
        sessions.cancel(2, "conv", &id);
        sessions.append(1, "conv", &id, 0, b"abc").unwrap();
    }

    #[test]
    fn a_user_may_hold_only_a_few_uploads_open() {
        let sessions = UploadSessions::new();
        for _ in 0..MAX_ACTIVE_UPLOADS_PER_USER {
            start(&sessions, 1, 10);
        }
        assert!(matches!(
            sessions.start(1, "conv", "a", None, 10, LIMIT),
            Err(UploadError::TooManyActive { user_id: 1, .. })
        ));
        // Someone else is not affected.
        start(&sessions, 2, 10);
    }

    #[test]
    fn reservations_are_capped_and_returned_on_cancel() {
        let sessions = UploadSessions::new();
        let per_file = 16 * 1024 * 1024;
        let mut ids = Vec::new();
        for user in 0..4 {
            let id = sessions
                .start(user, "conv", "a", None, per_file, per_file)
                .unwrap();
            ids.push((user, id));
        }
        assert!(matches!(
            sessions.start(9, "conv", "a", None, 1, per_file),
            Err(UploadError::OutOfCapacity { .. })
        ));
        let (user, id) = &ids[0];
        sessions.cancel(*user, "conv", id);
        sessions.start(9, "conv", "a", None, 1, per_file).unwrap();
    }

    #[test]
    fn an_abandoned_upload_expires_and_frees_its_reservation() {
        let sessions = UploadSessions::new();
        let t0 = Instant::now();
        let id = sessions
            .start_at(t0, 1, "conv", "a", None, 3, LIMIT)
            .unwrap();
        let later = t0 + SESSION_TTL + Duration::from_secs(1);
        assert!(matches!(
            sessions.append_at(later, 1, "conv", &id, 0, b"abc"),
            Err(UploadError::NotFound { .. })
        ));
        // And a new start sweeps expired sessions out of the user's count.
        for _ in 0..MAX_ACTIVE_UPLOADS_PER_USER {
            sessions
                .start_at(t0, 1, "conv", "a", None, 3, LIMIT)
                .unwrap();
        }
        sessions
            .start_at(later, 1, "conv", "a", None, 3, LIMIT)
            .unwrap();
    }
}
