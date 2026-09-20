#[derive(Default)]
pub(super) struct Observer {
    enabled: bool,
    request_goal: usize,
    request_accepted: usize,
    first_response_delivery: Option<(usize, usize)>,
    batches: usize,
    missed_batches: usize,
}

impl Observer {
    pub fn new(enabled: bool, request_goal: usize) -> Self {
        if enabled {
            assert!(request_goal > 0);
        }
        Self {
            enabled,
            request_goal,
            ..Self::default()
        }
    }

    pub fn begin_batch(&mut self) {
        self.request_accepted = 0;
        self.first_response_delivery = None;
    }

    pub fn transport_accepted(&mut self, payload: usize) {
        if self.enabled {
            self.request_accepted += payload;
            assert!(self.request_accepted <= self.request_goal);
        }
    }

    pub fn response_delivered(&mut self, payload: usize) {
        if self.enabled && payload > 0 {
            self.first_response_delivery
                .get_or_insert((self.request_accepted, payload));
        }
    }

    pub fn complete_batch(&mut self) {
        if !self.enabled {
            return;
        }
        assert_eq!(self.request_accepted, self.request_goal);
        let (accepted_at_delivery, response_payload) = self
            .first_response_delivery
            .expect("duplex workload must deliver actual response payload");
        self.batches += 1;
        let overlapped = accepted_at_delivery < self.request_goal;
        self.missed_batches += usize::from(!overlapped);
        eprintln!(
            "duplex_overlap batch={} response_payload={} request_transport_accepted={}/{} overlapped={}",
            self.batches, response_payload, accepted_at_delivery, self.request_goal, overlapped
        );
    }

    pub fn assert_complete(&self) {
        if self.enabled {
            assert!(self.batches > 0);
            eprintln!(
                "duplex_overlap post_shutdown complete_batches={} request_payload_accepted={} missed_batches={}",
                self.batches,
                self.batches * self.request_goal,
                self.missed_batches
            );
            assert_eq!(
                self.missed_batches, 0,
                "duplex overlap missing in {}/{} batches: response payload arrived only after all request payload transport acceptance",
                self.missed_batches, self.batches
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_actual_delivery_before_transport_completion() {
        let mut observer = Observer::new(true, 100);
        observer.begin_batch();
        observer.transport_accepted(40);
        observer.response_delivered(10);
        observer.transport_accepted(60);
        observer.complete_batch();
        observer.assert_complete();
    }

    #[test]
    #[should_panic(expected = "duplex overlap missing")]
    fn rejects_late_delivery_without_any_sent_or_ended_notification() {
        let mut observer = Observer::new(true, 100);
        observer.begin_batch();
        observer.transport_accepted(100);
        observer.response_delivered(10);
        observer.complete_batch();
        observer.assert_complete();
    }

    #[test]
    #[should_panic(expected = "duplex overlap missing in 1/2 batches")]
    fn zero_payload_or_previous_cohort_cannot_mask_late_delivery() {
        let mut observer = Observer::new(true, 100);
        observer.begin_batch();
        observer.transport_accepted(40);
        observer.response_delivered(10);
        observer.transport_accepted(60);
        observer.complete_batch();
        observer.begin_batch();
        observer.transport_accepted(40);
        observer.response_delivered(0);
        observer.transport_accepted(60);
        observer.response_delivered(10);
        observer.complete_batch();
        observer.assert_complete();
    }
}
