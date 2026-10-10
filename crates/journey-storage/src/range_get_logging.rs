use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};

#[derive(Clone, Debug)]
pub struct RangeGetAggregator {
    groups: Arc<Mutex<HashMap<(String, String), GroupActivity>>>,
}

pub struct RangeGetTask {
    groups: Arc<Mutex<HashMap<(String, String), GroupActivity>>>,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy)]
pub enum RangeGetOutcome {
    Completed,
    Cancelled,
    Failed,
}

struct RangeKey {
    key: String,
    representation: String,
}

#[derive(Debug, Default)]
struct GroupActivity {
    active_requests: HashSet<u64>,
    started: u64,
    requests_total: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,
    bytes: u64,
}

struct RangeSummary {
    key: String,
    representation: String,
    started: u64,
    requests_total: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,
    bytes: u64,
    active_requests: usize,
}

impl RangeGetAggregator {
    pub fn started(&self, request_id: u64, key: &str, representation: &str) {
        let group = RangeKey { key: key.to_owned(), representation: representation.to_owned() };
        let mut groups = self.groups.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        record_started(&mut groups, request_id, group);
        drop(groups);
        log::debug!(
            "storage_range_get_started request_id={request_id} key={key:?} representation={representation}"
        );
    }

    pub fn bytes_sent(&self, request_id: u64, key: &str, representation: &str, bytes: u64) {
        let mut groups = self.groups.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        record_bytes(&mut groups, request_id, key, representation, bytes);
    }

    pub fn finished(
        &self,
        request_id: u64,
        key: &str,
        representation: &str,
        outcome: RangeGetOutcome,
        bytes: u64,
    ) {
        let group = RangeKey { key: key.to_owned(), representation: representation.to_owned() };
        let mut groups = self.groups.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        record_finished(&mut groups, request_id, group, outcome);
        drop(groups);
        let outcome = match outcome {
            RangeGetOutcome::Completed => "completed",
            RangeGetOutcome::Cancelled => "cancelled",
            RangeGetOutcome::Failed => "failed",
        };
        log::debug!(
            "storage_range_get_finished request_id={request_id} key={key:?} representation={representation} outcome={outcome} bytes={bytes}"
        );
    }
}

impl RangeGetTask {
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
        emit_summaries(&self.groups);
    }
}

pub fn start_range_get_aggregator(
    summary_interval: Duration,
) -> io::Result<(RangeGetAggregator, RangeGetTask)> {
    if summary_interval.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "range aggregation interval must be positive",
        ));
    }
    let groups = Arc::new(Mutex::new(HashMap::new()));
    let task = tokio::spawn(run_aggregator(Arc::clone(&groups), summary_interval));
    Ok((RangeGetAggregator { groups: Arc::clone(&groups) }, RangeGetTask { groups, task }))
}

async fn run_aggregator(
    groups: Arc<Mutex<HashMap<(String, String), GroupActivity>>>,
    summary_interval: Duration,
) {
    let mut interval = time::interval_at(time::Instant::now() + summary_interval, summary_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        emit_summaries(&groups);
    }
}

fn record_started(
    groups: &mut HashMap<(String, String), GroupActivity>,
    request_id: u64,
    group: RangeKey,
) {
    let activity = groups.entry((group.key, group.representation)).or_default();
    activity.started += 1;
    activity.requests_total += 1;
    activity.active_requests.insert(request_id);
}

fn record_finished(
    groups: &mut HashMap<(String, String), GroupActivity>,
    request_id: u64,
    group: RangeKey,
    outcome: RangeGetOutcome,
) {
    let activity = groups.entry((group.key, group.representation)).or_default();
    activity.active_requests.remove(&request_id);
    match outcome {
        RangeGetOutcome::Completed => activity.completed += 1,
        RangeGetOutcome::Cancelled => activity.cancelled += 1,
        RangeGetOutcome::Failed => activity.failed += 1,
    }
}

fn record_bytes(
    groups: &mut HashMap<(String, String), GroupActivity>,
    request_id: u64,
    key: &str,
    representation: &str,
    bytes: u64,
) {
    if let Some(activity) = groups.get_mut(&(key.to_owned(), representation.to_owned())) {
        if activity.active_requests.contains(&request_id) {
            activity.bytes = activity.bytes.saturating_add(bytes);
        }
    }
}

fn emit_summaries(groups: &Mutex<HashMap<(String, String), GroupActivity>>) {
    let summaries = {
        let mut groups = groups.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut summaries = Vec::new();
        groups.retain(|(key, representation), activity| {
            let active_requests = activity.active_requests.len();
            if activity.started > 0 || activity.completed > 0 || activity.cancelled > 0
                || activity.failed > 0 || activity.bytes > 0 || active_requests > 0
            {
                summaries.push(RangeSummary {
                    key: key.clone(),
                    representation: representation.clone(),
                    started: activity.started,
                    requests_total: activity.requests_total,
                    completed: activity.completed,
                    cancelled: activity.cancelled,
                    failed: activity.failed,
                    bytes: activity.bytes,
                    active_requests,
                });
            }
            activity.started = 0;
            activity.completed = 0;
            activity.cancelled = 0;
            activity.failed = 0;
            activity.bytes = 0;
            active_requests > 0
        });
        summaries
    };
    for summary in summaries {
        log::info!(
            "storage_range_get_summary key={:?} representation={} started={} requests_total={} completed={} cancelled={} failed={} bytes_handed_to_transport={} active_requests={}",
            summary.key,
            summary.representation,
            summary.started,
            summary.requests_total,
            summary.completed,
            summary.cancelled,
            summary.failed,
            summary.bytes,
            summary.active_requests,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(key: &str, representation: &str) -> RangeKey {
        RangeKey { key: key.to_owned(), representation: representation.to_owned() }
    }

    #[test]
    fn range_requests_share_groups_by_key_and_representation() {
        let mut groups = HashMap::new();
        record_started(&mut groups, 1, group("video.mp4", "original"));
        record_started(&mut groups, 2, group("video.mp4", "original"));
        record_started(&mut groups, 3, group("video.mp4", "thumbnail"));
        record_bytes(&mut groups, 1, "video.mp4", "original", 128);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&(String::from("video.mp4"), String::from("original"))].started, 2);
        assert_eq!(groups[&(String::from("video.mp4"), String::from("original"))].active_requests.len(), 2);

        record_finished(
            &mut groups,
            1,
            group("video.mp4", "original"),
            RangeGetOutcome::Cancelled,
        );
        let original = &groups[&(String::from("video.mp4"), String::from("original"))];
        assert_eq!(original.cancelled, 1);
        assert_eq!(original.bytes, 128);
        assert_eq!(original.active_requests.len(), 1);
    }

    #[test]
    fn summary_flush_resets_counters_and_removes_inactive_groups() {
        let groups = Mutex::new(HashMap::new());
        {
            let mut groups = groups.lock().unwrap();
            record_started(&mut groups, 1, group("video.mp4", "original"));
            record_bytes(&mut groups, 1, "video.mp4", "original", 512);
            record_finished(
                &mut groups,
                1,
                group("video.mp4", "original"),
                RangeGetOutcome::Completed,
            );
        }
        emit_summaries(&groups);
        assert!(groups.lock().unwrap().is_empty());

        {
            let mut groups = groups.lock().unwrap();
            record_started(&mut groups, 2, group("video.mp4", "original"));
        }
        emit_summaries(&groups);
        let groups = groups.lock().unwrap();
        let original = &groups[&(String::from("video.mp4"), String::from("original"))];
        assert_eq!(original.started, 0);
        assert_eq!(original.active_requests.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_emits_final_range_summaries() {
        let (aggregator, task) = start_range_get_aggregator(Duration::from_secs(60)).unwrap();
        aggregator.started(1, "video.mp4", "original");
        aggregator.finished(1, "video.mp4", "original", RangeGetOutcome::Completed, 64);
        task.shutdown().await;
    }
}
