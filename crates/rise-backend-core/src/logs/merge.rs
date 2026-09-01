//! Merging one stream per container into the single stream the API returns.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use super::{LogEvent, LogEventStream, TimestampedLineStream, DOCKER_MAX_TAIL};
use crate::logs::cursor::stable_log_id;

/// Merge per-container line streams into the one stream the API returns.
///
/// The two modes differ because the guarantees available differ, not for
/// convenience:
///
/// - **Following**, lines are emitted as they arrive. A live merge cannot be
///   globally ordered without buffering, and buffering a follow holds back the
///   output that is the point of following. `docker compose logs -f` and
///   `kubectl logs -f --all-containers` make the same trade; the container named
///   on each line is what lets a reader put them back together.
/// - **Not following**, the whole range is in hand, so it is sorted by
///   timestamp before anything is emitted.
///
/// `tail_limit` is applied *after* the merge. It asks for N lines from the
/// deployment, but each container's stream was asked for N of its own, so
/// without this a four-container deployment returns four times what was asked.
pub fn merge_container_streams(
    streams: Vec<TimestampedLineStream>,
    follow: bool,
    tail_limit: Option<usize>,
) -> LogEventStream {
    if follow {
        return futures::stream::select_all(streams)
            .map(|item| item.map(|(_, event)| event))
            .boxed();
    }

    // Only the newest `buffer_cap` lines can survive the trim below, so the
    // buffer holds that many and no more. Collecting first and trimming after
    // would size the server's memory from the request: N containers each
    // answering a large `tail` at once.
    let buffer_cap = tail_limit
        .unwrap_or(DOCKER_MAX_TAIL as usize)
        .clamp(1, DOCKER_MAX_TAIL as usize);

    async_stream::stream! {
        let mut newest = BoundedNewest::new(buffer_cap);
        let mut arrivals = 0usize;
        let mut merged = futures::stream::select_all(streams);
        while let Some(item) = merged.next().await {
            match item {
                Ok((timestamp, event)) => {
                    // This page is not paginated (`next_cursor` is always
                    // `None` below), so arrival order is a sufficient tiebreak
                    // here — no later request has to agree with it.
                    let key = MergeKey { timestamp, source: 0, sequence: arrivals };
                    arrivals = arrivals.saturating_add(1);
                    newest.push(key, event);
                }
                // A container that failed has said so; the lines the others
                // produced are still worth returning.
                Err(e) => yield Err(e),
            }
        }

        for event in newest.into_chronological() {
            yield Ok(event);
        }
        yield Ok(LogEvent::PageLoaded { next_cursor: None });
    }
    .boxed()
}

/// Total order for merged lines: time first, then a tiebreak that does not
/// depend on which source happened to answer first.
///
/// The tiebreak carries pagination correctness. Two pages are two requests, and
/// if lines sharing a timestamp sorted differently between them, one would be
/// served twice and another skipped — so `source`/`sequence` identify the
/// stream and the line's place within it, never its arrival.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct MergeKey {
    pub timestamp: DateTime<Utc>,
    /// Which container's stream the line came from. Breaks ties between lines
    /// sharing a timestamp so the merge is deterministic across runs.
    pub source: usize,
    /// Position within that container's stream, breaking ties within a source.
    pub sequence: usize,
}

/// The newest `capacity` lines seen across every source, in bounded memory.
///
/// A merge cannot know which lines are the newest until every source has been
/// read, but it does know that anything older than the `capacity` newest so far
/// can never become one. Dropping those as they arrive keeps the buffer the
/// size of the answer rather than the size of the input — so it does not grow
/// with the number of containers or Pods, and a deployment with fifty costs
/// what one with two costs.
pub struct BoundedNewest<T> {
    capacity: usize,
    heap: BinaryHeap<Reverse<Keyed<T>>>,
}

struct Keyed<T> {
    key: MergeKey,
    value: T,
}

impl<T> PartialEq for Keyed<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<T> Eq for Keyed<T> {}
impl<T> PartialOrd for Keyed<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Keyed<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl<T> BoundedNewest<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            heap: BinaryHeap::new(),
        }
    }

    pub fn push(&mut self, key: MergeKey, value: T) {
        self.heap.push(Reverse(Keyed { key, value }));
        if self.heap.len() > self.capacity {
            // `Reverse` puts the oldest line at the heap's root, which is
            // exactly the one that just stopped being a candidate.
            self.heap.pop();
        }
    }

    pub fn into_chronological(self) -> Vec<T> {
        let mut kept = self.heap.into_vec();
        kept.sort_by(|Reverse(left), Reverse(right)| left.key.cmp(&right.key));
        kept.into_iter().map(|Reverse(keyed)| keyed.value).collect()
    }
}

pub fn distinct_log_id(seen: &mut HashMap<String, u64>, base_id: String) -> String {
    let occurrence = seen.entry(base_id.clone()).or_default();
    let id = if *occurrence == 0 {
        base_id.clone()
    } else {
        let occurrence_bytes = occurrence.to_be_bytes();
        stable_log_id(
            "occurrence",
            [base_id.as_bytes(), occurrence_bytes.as_slice()],
        )
    };
    *occurrence = occurrence.saturating_add(1);
    id
}

pub fn split_timestamped_log_line(line: &str) -> Option<(DateTime<Utc>, &str, &str)> {
    let (timestamp_text, content) = line.split_once(' ')?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp_text)
        .ok()?
        .with_timezone(&Utc);
    Some((timestamp, content, timestamp_text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::cursor::stable_log_id;

    fn at(second: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-08-31T12:00:{second:02}Z"))
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn bounded_merge_keeps_the_newest_and_not_the_first_seen() {
        // Sources are read concurrently, so the oldest line can arrive last.
        // Keeping the first `capacity` would return whatever raced in first.
        let mut newest = BoundedNewest::new(3);
        for (source, second) in [(1, 9), (0, 1), (1, 7), (0, 3), (1, 5)] {
            newest.push(
                MergeKey {
                    timestamp: at(second),
                    source,
                    sequence: 0,
                },
                format!("line-{second}"),
            );
        }

        assert_eq!(
            newest.into_chronological(),
            vec!["line-5", "line-7", "line-9"]
        );
    }

    #[test]
    fn bounded_merge_does_not_grow_with_the_number_of_sources() {
        // The property that keeps one request's memory off the replica count:
        // twenty Pods of a hundred lines each still cost one page, not twenty.
        let mut newest = BoundedNewest::new(10);
        for source in 0..20 {
            for sequence in 0..100 {
                newest.push(
                    MergeKey {
                        timestamp: at((sequence % 60) as u32),
                        source,
                        sequence,
                    },
                    format!("{source}:{sequence}"),
                );
            }
        }

        assert_eq!(newest.into_chronological().len(), 10);
    }

    #[test]
    fn bounded_merge_result_does_not_depend_on_arrival_order() {
        // Two pages are two requests, and they must agree on how lines sharing
        // a timestamp are ordered. If they disagreed, the page boundary would
        // serve one line twice and step over another.
        let lines = [(0, 0, 5), (1, 0, 5), (0, 1, 5), (1, 1, 5)];
        let collect = |order: &[usize]| {
            let mut newest = BoundedNewest::new(10);
            for &index in order {
                let (source, sequence, second) = lines[index];
                newest.push(
                    MergeKey {
                        timestamp: at(second),
                        source,
                        sequence,
                    },
                    format!("{source}:{sequence}"),
                );
            }
            newest.into_chronological()
        };

        assert_eq!(collect(&[0, 1, 2, 3]), collect(&[3, 1, 0, 2]));
        assert_eq!(collect(&[0, 1, 2, 3]), vec!["0:0", "0:1", "1:0", "1:1"]);
    }

    #[test]
    fn timestamped_line_identity_is_distinct_per_occurrence() {
        let line = "2026-08-29T12:34:56.123456789Z repeated";
        let (timestamp, content, timestamp_text) = split_timestamped_log_line(line).unwrap();
        assert_eq!(
            timestamp.timestamp_nanos_opt(),
            Some(1_788_006_896_123_456_789)
        );
        assert_eq!(content, "repeated");

        let base_id = stable_log_id(
            "kubernetes",
            [timestamp_text.as_bytes(), content.as_bytes()],
        );
        let mut seen = HashMap::new();
        let first = distinct_log_id(&mut seen, base_id.clone());
        let second = distinct_log_id(&mut seen, base_id.clone());
        assert_ne!(first, second);

        let mut retry = HashMap::new();
        assert_eq!(first, distinct_log_id(&mut retry, base_id.clone()));
        assert_eq!(second, distinct_log_id(&mut retry, base_id));
    }
}
