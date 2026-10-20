// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{
    apply_resolved_to_json, artifacts::canonical_debug_id, extract_source_context, ResolvedFrame,
    SourceMapService,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::Semaphore;

static SYMBOLICATION_WORKERS: Semaphore = Semaphore::const_new(2);

impl SourceMapService {
    /// A single bounded batch lookup and one parse per map, rather than one
    /// database lookup/parse per frame. Missing/busy maps leave the event intact
    /// and can be resolved on a later native detail read after upload completes.
    pub(super) async fn symbolicate_debug_ids(&self, project_id: i32, event: &mut Value) {
        let Some(images) = event
            .pointer("/debug_meta/images")
            .and_then(Value::as_array)
        else {
            return;
        };
        let mut associations = BTreeMap::new();
        for image in images {
            if image.get("type").and_then(Value::as_str) != Some("sourcemap") {
                continue;
            }
            if let (Some(path), Some(id)) = (
                image.get("code_file").and_then(Value::as_str),
                image
                    .get("debug_id")
                    .and_then(Value::as_str)
                    .and_then(canonical_debug_id),
            ) {
                if path.len() <= 4096 && associations.len() < 1000 {
                    associations.insert(path.to_owned(), id);
                }
            }
        }
        if associations.is_empty() {
            return;
        }
        // Annotate identities even if artifacts are missing. Release-based
        // fallback must never apply an unrelated build to an exact Debug ID.
        visit_frames(event, &mut |frame| {
            let path = frame
                .get("abs_path")
                .or_else(|| frame.get("filename"))
                .and_then(Value::as_str);
            if let Some(id) = path.and_then(|p| associations.get(p)) {
                frame.insert("debug_id".into(), Value::String(id.clone()));
            }
        });
        let Ok(_permit) = SYMBOLICATION_WORKERS.try_acquire() else {
            return;
        };
        let ids = associations.values().take(32).cloned().collect::<Vec<_>>();
        let rows = match self
            .db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT debug_id,source_map_data FROM (
               SELECT debug_id,source_map_data,sum(size_bytes) OVER(ORDER BY debug_id) AS total
               FROM source_maps WHERE project_id=$1 AND debug_id=ANY($2)
             ) maps WHERE total<=52428800",
                [project_id.into(), ids.into()],
            ))
            .await
        {
            Ok(rows) => rows,
            Err(_) => {
                tracing::warn!(project_id, "Debug ID source map lookup failed");
                return;
            }
        };
        let mut bytes = Vec::new();
        for row in rows {
            if let (Ok(id), Ok(data)) = (
                row.try_get::<String>("", "debug_id"),
                row.try_get::<Vec<u8>>("", "source_map_data"),
            ) {
                bytes.push((id, data));
            }
        }
        let Ok(maps) = tokio::task::spawn_blocking(move || {
            bytes
                .into_iter()
                .filter_map(|(id, data)| {
                    sourcemap::SourceMap::from_slice(&data)
                        .ok()
                        .map(|map| (id, map))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .await
        else {
            return;
        };
        visit_frames(event, &mut |frame| {
            if frame.get("symbolicated").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let Some(sm) = frame
                .get("debug_id")
                .and_then(Value::as_str)
                .and_then(|id| maps.get(id))
            else {
                return;
            };
            let Some(line) = frame
                .get("lineno")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
            else {
                return;
            };
            let column = frame
                .get("colno")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            let Some(token) = sm.lookup_token(line - 1, column.saturating_sub(1)) else {
                return;
            };
            let filename = frame
                .get("abs_path")
                .or_else(|| frame.get("filename"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let source_line = token.get_src_line() + 1;
            let (context_line, pre_context, post_context) = sm
                .get_source_contents(token.get_src_id())
                .map(|s| extract_source_context(s, source_line))
                .unwrap_or_default();
            apply_resolved_to_json(
                frame,
                ResolvedFrame {
                    file: token.get_source().unwrap_or(&filename).to_owned(),
                    line: source_line,
                    column: token.get_src_col() + 1,
                    function: token.get_name().map(str::to_owned),
                    context_line,
                    pre_context,
                    post_context,
                },
                &filename,
            );
        });
    }
}

fn visit_frames(event: &mut Value, visitor: &mut impl FnMut(&mut serde_json::Map<String, Value>)) {
    // Explicit protocol paths keep this independent of arbitrary deeply nested
    // contexts/extra values. The envelope already bounds frame JSON bytes.
    for collection in ["exception", "threads"] {
        if let Some(values) = event
            .get_mut(collection)
            .and_then(|v| v.get_mut("values"))
            .and_then(Value::as_array_mut)
        {
            for value in values.iter_mut().take(1000) {
                visit_stack(value.get_mut("stacktrace"), visitor);
            }
        }
    }
    visit_stack(event.get_mut("stacktrace"), visitor);
}
fn visit_stack(
    stack: Option<&mut Value>,
    visitor: &mut impl FnMut(&mut serde_json::Map<String, Value>),
) {
    if let Some(frames) = stack
        .and_then(|s| s.get_mut("frames"))
        .and_then(Value::as_array_mut)
    {
        for frame in frames
            .iter_mut()
            .take(1000)
            .filter_map(Value::as_object_mut)
        {
            visitor(frame);
        }
    }
}
