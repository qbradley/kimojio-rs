use super::*;
use std::alloc::{GlobalAlloc, Layout, System};

thread_local! {
    static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct TrackingAllocator;
// The wrapper preserves System's allocation and deallocation contracts.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn allocations(f: impl FnOnce()) -> usize {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            TRACK_ALLOCATIONS.set(false);
        }
    }
    ALLOCATIONS.set(0);
    TRACK_ALLOCATIONS.set(true);
    let guard = Guard;
    f();
    drop(guard);
    ALLOCATIONS.get()
}

fn config() -> Config {
    Config {
        max_clients: 3,
        max_message_bytes: 1024,
        max_messages_per_client: 4,
        max_client_bytes: 8192,
        max_total_bytes: 1024 * 1024,
        external_bytes_per_client: 128,
        external_fixed_bytes: 0,
    }
}

fn add(hub: &mut Hub) -> ClientId {
    let id = hub.admit().unwrap();
    hub.activate(id).unwrap();
    id
}

fn publish(hub: &mut Hub, id: ClientId, kind: Kind, data: &[u8]) -> Published {
    hub.begin(id, kind).unwrap();
    hub.append(id, data).unwrap();
    hub.finish(id).unwrap()
}

#[derive(Debug)]
enum Event {
    Send(Delivery),
    Close(Close),
    Yield,
}

struct Escaping;
impl Ports for Escaping {
    type Output = Event;
    fn send(&mut self, delivery: Delivery) -> Option<Event> {
        Some(Event::Send(delivery))
    }
    fn close(&mut self, close: Close) -> Option<Event> {
        Some(Event::Close(close))
    }
    fn yield_turn(&mut self) -> Option<Event> {
        Some(Event::Yield)
    }
}

fn send(hub: &mut Hub) -> Delivery {
    match hub.next(&mut Escaping).unwrap() {
        Event::Send(delivery) => delivery,
        event => panic!("expected delivery, got {event:?}"),
    }
}

fn close(hub: &mut Hub) -> Close {
    match hub.next(&mut Escaping).unwrap() {
        Event::Close(close) => close,
        event => panic!("expected close, got {event:?}"),
    }
}

#[test]
fn finite_admission_and_upgrade_activation_are_distinct() {
    let mut hub = Hub::new(7, config()).unwrap();
    let a = add(&mut hub);
    let pending = hub.admit().unwrap();
    let c = add(&mut hub);
    assert_eq!(hub.admit(), Err(Error::AdmissionLimit));
    assert_eq!(hub.begin(pending, Kind::Text), Err(Error::InvalidState));
    assert_eq!(hub.stats().active_clients, 2);
    assert_eq!(publish(&mut hub, a, Kind::Text, b"hello").recipients, 2);
    let first = send(&mut hub);
    let second = send(&mut hub);
    assert_eq!([first.id.client(), second.id.client()], [a, c]);
    hub.complete(first.complete(DeliveryResult::Sent)).unwrap();
    hub.complete(second.complete(DeliveryResult::Sent)).unwrap();
    assert!(hub.next(&mut Escaping).is_none());
    hub.activate(pending).unwrap();
    assert_eq!(hub.activate(pending), Err(Error::InvalidState));
    assert_eq!(
        publish(&mut hub, pending, Kind::Binary, b"next").recipients,
        3
    );
}

#[test]
fn sender_included_and_one_order_preserved_for_every_recipient() {
    let mut hub = Hub::new(1, config()).unwrap();
    let ids = [add(&mut hub), add(&mut hub), add(&mut hub)];
    let first = publish(&mut hub, ids[0], Kind::Text, b"first");
    let second = publish(&mut hub, ids[1], Kind::Binary, b"second");
    assert!(second.order > first.order);
    for (order, bytes, kind) in [
        (first.order, &b"first"[..], Kind::Text),
        (second.order, &b"second"[..], Kind::Binary),
    ] {
        let deliveries = [send(&mut hub), send(&mut hub), send(&mut hub)];
        assert!(hub.next(&mut Escaping).is_none());
        for (id, delivery) in ids.into_iter().zip(deliveries) {
            assert_eq!(delivery.id.client(), id);
            assert_eq!(delivery.payload.order(), order);
            assert_eq!(delivery.payload.as_ref(), bytes);
            assert_eq!(delivery.payload.kind(), kind);
            hub.complete(delivery.complete(DeliveryResult::Sent))
                .unwrap();
        }
    }
}

#[test]
fn payload_is_shared_once_and_lease_prevents_early_release() {
    let mut hub = Hub::new(1, config()).unwrap();
    let ids = [add(&mut hub), add(&mut hub), add(&mut hub)];
    let baseline = hub.stats().used_bytes;
    publish(&mut hub, ids[0], Kind::Binary, b"x");
    let charged = 1 + size_of::<Payload>() + RC_COUNTS;
    assert_eq!(hub.stats().used_bytes, baseline + charged);
    let a = send(&mut hub);
    let b = send(&mut hub);
    let c = send(&mut hub);
    assert_eq!(a.payload.as_ref().as_ptr(), b.payload.as_ref().as_ptr());
    assert_eq!(b.payload.as_ref().as_ptr(), c.payload.as_ref().as_ptr());
    hub.complete(a.complete(DeliveryResult::Sent)).unwrap();
    hub.complete(b.complete(DeliveryResult::Sent)).unwrap();
    assert_eq!(hub.stats().used_bytes, baseline + charged);
    hub.complete(c.complete(DeliveryResult::Sent)).unwrap();
    assert_eq!(hub.stats().used_bytes, baseline);
}

#[test]
fn empty_messages_allocate_no_payload_or_shared_metadata() {
    let mut hub = Hub::new(1, config()).unwrap();
    let id = add(&mut hub);
    let baseline = hub.stats().used_bytes;
    for _ in 0..4 {
        publish(&mut hub, id, Kind::Text, b"");
    }
    assert_eq!(hub.stats().used_bytes, baseline);
    for _ in 0..4 {
        let delivery = send(&mut hub);
        assert_eq!(delivery.payload.capacity(), 0);
        assert_eq!(delivery.payload.as_ref(), b"");
        hub.complete(delivery.complete(DeliveryResult::Sent))
            .unwrap();
    }
    assert_eq!(hub.stats().peak_bytes, baseline);
}

#[test]
fn allocator_observes_zero_empty_and_two_tiny_message_allocations() {
    let mut hub = Hub::new(1, config()).unwrap();
    let id = add(&mut hub);
    add(&mut hub);
    add(&mut hub);
    assert_eq!(
        allocations(|| {
            publish(&mut hub, id, Kind::Text, b"");
            for _ in 0..3 {
                let delivery = send(&mut hub);
                hub.complete(delivery.complete(DeliveryResult::Sent))
                    .unwrap();
            }
        }),
        0
    );
    assert_eq!(
        allocations(|| {
            publish(&mut hub, id, Kind::Binary, b"x");
        }),
        2
    );
}

#[test]
fn aggregate_assembly_failure_keeps_other_publishers_and_assemblies() {
    let mut reference = Hub::new(1, config()).unwrap();
    add(&mut reference);
    add(&mut reference);
    let admitted = reference.stats().used_bytes;
    let mut options = config();
    options.max_total_bytes = admitted + 4 + size_of::<Payload>() + RC_COUNTS;
    let mut hub = Hub::new(1, options).unwrap();
    let a = add(&mut hub);
    let b = add(&mut hub);
    hub.begin(a, Kind::Text).unwrap();
    hub.append(a, b"safe").unwrap();
    hub.begin(b, Kind::Binary).unwrap();
    assert_eq!(hub.append(b, &[0; 1024]), Err(Error::AggregateLimit));
    assert_eq!(
        close(&mut hub),
        Close {
            client: b,
            code: 1008
        }
    );
    assert_eq!(hub.finish(a).unwrap().recipients, 1);
    let delivery = send(&mut hub);
    assert_eq!(delivery.id.client(), a);
    assert_eq!(delivery.payload.as_ref(), b"safe");
    hub.complete(delivery.complete(DeliveryResult::Sent))
        .unwrap();
    assert_eq!(hub.stats().active_clients, 1);
}

#[test]
fn text_is_validated_only_after_complete_assembly() {
    let mut hub = Hub::new(1, config()).unwrap();
    let id = add(&mut hub);
    hub.begin(id, Kind::Text).unwrap();
    hub.append(id, &[0xf0, 0x9f]).unwrap();
    assert!(hub.next(&mut Escaping).is_none());
    hub.append(id, &[0x98, 0x80]).unwrap();
    hub.finish(id).unwrap();
    let delivery = send(&mut hub);
    assert_eq!(delivery.payload.as_ref(), "😀".as_bytes());
    hub.complete(delivery.complete(DeliveryResult::Sent))
        .unwrap();
    hub.begin(id, Kind::Text).unwrap();
    hub.append(id, &[0xc0, 0xaf]).unwrap();
    assert_eq!(hub.finish(id), Err(Error::InvalidText));
    assert_eq!(
        close(&mut hub),
        Close {
            client: id,
            code: 1007
        }
    );
    assert!(hub.next(&mut Escaping).is_none());
}

fn baseline() -> usize {
    let mut hub = Hub::new(1, config()).unwrap();
    add(&mut hub);
    hub.stats().used_bytes
}

#[test]
fn growth_reserves_old_plus_new_capacity_before_allocation() {
    let mut options = config();
    options.max_total_bytes = baseline() + 7;
    let mut hub = Hub::new(1, options).unwrap();
    let id = add(&mut hub);
    let baseline = hub.stats().used_bytes;
    hub.begin(id, Kind::Binary).unwrap();
    hub.append(id, b"ab").unwrap(); // old capacity 2
    hub.append(id, b"c").unwrap(); // old 2 + new 4 fit
    assert_eq!(hub.stats().used_bytes, baseline + 4);
    assert_eq!(hub.stats().peak_bytes, baseline + 6);
    assert_eq!(hub.append(id, b"de"), Err(Error::AggregateLimit)); // old 4 + new 8 fail
    assert_eq!(hub.stats().used_bytes, baseline);
    assert_eq!(close(&mut hub).code, 1008);
    assert!(hub.next(&mut Escaping).is_none());
}

#[test]
fn shared_header_allocation_failure_prevents_any_publication() {
    let mut options = config();
    options.max_total_bytes = baseline() + 1;
    let mut hub = Hub::new(1, options).unwrap();
    let id = add(&mut hub);
    hub.begin(id, Kind::Binary).unwrap();
    hub.append(id, b"x").unwrap();
    assert_eq!(hub.finish(id), Err(Error::AggregateLimit));
    assert_eq!(close(&mut hub).code, 1008);
    assert!(hub.next(&mut Escaping).is_none());
    assert_eq!(hub.stats().used_bytes, baseline());
}

#[test]
fn message_size_failure_removes_only_publisher_before_broadcast() {
    let mut hub = Hub::new(1, config()).unwrap();
    let a = add(&mut hub);
    let b = add(&mut hub);
    hub.begin(a, Kind::Binary).unwrap();
    assert_eq!(hub.append(a, &[0; 1025]), Err(Error::MessageLimit));
    assert_eq!(
        close(&mut hub),
        Close {
            client: a,
            code: 1008
        }
    );
    assert_eq!(hub.stats().active_clients, 1);
    assert_eq!(publish(&mut hub, b, Kind::Text, b"healthy").recipients, 1);
    assert_eq!(send(&mut hub).id.client(), b);
}

#[test]
fn slow_recipient_overflow_includes_inflight_and_continues_others() {
    let mut options = config();
    options.max_messages_per_client = 2;
    let mut hub = Hub::new(1, options).unwrap();
    let slow = add(&mut hub);
    let fast = add(&mut hub);
    publish(&mut hub, fast, Kind::Text, b"one");
    let retained = send(&mut hub);
    assert_eq!(retained.id.client(), slow);
    let first_fast = send(&mut hub);
    hub.complete(first_fast.complete(DeliveryResult::Sent))
        .unwrap();
    publish(&mut hub, fast, Kind::Text, b"two");
    let second_fast = send(&mut hub);
    assert_eq!(second_fast.id.client(), fast);
    hub.complete(second_fast.complete(DeliveryResult::Sent))
        .unwrap();
    let result = publish(&mut hub, fast, Kind::Text, b"three");
    assert_eq!((result.recipients, result.removed), (1, 1));
    assert_eq!(
        close(&mut hub),
        Close {
            client: slow,
            code: 1008
        }
    );
    let third_fast = send(&mut hub);
    assert_eq!(third_fast.payload.as_ref(), b"three");
    hub.complete(third_fast.complete(DeliveryResult::Sent))
        .unwrap();
    hub.closed(slow).unwrap();
    assert_eq!(hub.stats().resident_clients, 2);
    hub.complete(retained.complete(DeliveryResult::Failed))
        .unwrap();
    assert_eq!(hub.stats().resident_clients, 1);
    assert!(hub.next(&mut Escaping).is_none());
}

#[test]
fn byte_limit_uses_allocated_capacity_not_only_length() {
    let mut options = config();
    options.max_message_bytes = 8;
    let metadata = options.max_messages_per_client * size_of::<Option<SharedPayload>>()
        + size_of::<DeliveryCompletion>().max(size_of::<Delivery>());
    options.max_client_bytes = metadata + 8;
    let mut hub = Hub::new(1, options).unwrap();
    let id = add(&mut hub);
    publish(&mut hub, id, Kind::Binary, b"12345"); // capacity 8, not 5
    let result = publish(&mut hub, id, Kind::Binary, b"x");
    assert_eq!((result.recipients, result.removed), (0, 1));
    assert_eq!(close(&mut hub).code, 1008);
}

#[test]
fn logical_close_and_original_completion_settle_in_either_order() {
    for completion_first in [false, true] {
        let mut options = config();
        options.max_clients = 1;
        let mut hub = Hub::new(1, options).unwrap();
        let empty = hub.stats().used_bytes;
        let id = add(&mut hub);
        let admitted = hub.stats().used_bytes;
        publish(&mut hub, id, Kind::Binary, b"123");
        let delivery = send(&mut hub);
        hub.remove(id, 1008).unwrap();
        assert_eq!(close(&mut hub).code, 1008);
        let charged = hub.stats().used_bytes;
        assert!(charged > admitted);
        assert_eq!(hub.admit(), Err(Error::AdmissionLimit));
        let completion = delivery.complete(DeliveryResult::Sent);
        if completion_first {
            hub.complete(completion).unwrap();
            assert_eq!(hub.stats().used_bytes, admitted);
            assert_eq!(hub.admit(), Err(Error::AdmissionLimit));
            hub.closed(id).unwrap();
        } else {
            hub.closed(id).unwrap();
            assert_eq!(hub.stats().used_bytes, charged);
            assert_eq!(hub.admit(), Err(Error::AdmissionLimit));
            hub.complete(completion).unwrap();
        }
        assert_eq!(hub.stats().used_bytes, empty);
        let replacement = add(&mut hub);
        assert_ne!(replacement, id);
        assert_eq!(hub.closed(id), Err(Error::UnknownClient));
        assert_eq!(hub.begin(id, Kind::Text), Err(Error::UnknownClient));
    }
}

#[test]
fn foreign_hub_completion_returns_owned_payload_unchanged() {
    let mut a = Hub::new(1, config()).unwrap();
    let mut b = Hub::new(1, config()).unwrap(); // even an incorrectly reused owner identity
    let aid = add(&mut a);
    let bid = add(&mut b);
    publish(&mut a, aid, Kind::Binary, b"a");
    publish(&mut b, bid, Kind::Binary, b"b");
    let first = send(&mut a).complete(DeliveryResult::Sent);
    let other = send(&mut b).complete(DeliveryResult::Sent);
    let returned = b.complete(first).unwrap_err();
    assert_eq!(returned.payload.as_ref(), b"a");
    a.complete(returned).unwrap();
    b.complete(other).unwrap();
}

#[test]
fn wrong_delivery_identity_is_rejected_without_settling_or_losing_storage() {
    let mut hub = Hub::new(1, config()).unwrap();
    let id = add(&mut hub);
    publish(&mut hub, id, Kind::Binary, b"payload");
    let mut receipt = send(&mut hub).complete(DeliveryResult::Sent);
    let original_id = receipt.id;
    receipt.id.order += 1;
    let charged = hub.stats().used_bytes;
    let mut returned = hub.complete(receipt).unwrap_err();
    assert_eq!(hub.stats().used_bytes, charged);
    returned.id = original_id;
    hub.complete(returned).unwrap();
    assert!(hub.next(&mut Escaping).is_none());
}

#[test]
fn readiness_is_unlinked_before_slot_reuse() {
    let mut hub = Hub::new(1, config()).unwrap();
    let a = add(&mut hub);
    let b = add(&mut hub);
    publish(&mut hub, a, Kind::Binary, b"old");
    hub.closed(a).unwrap();
    let c = add(&mut hub);
    assert_eq!(c.slot(), a.slot());
    let old = send(&mut hub);
    assert_eq!(old.id.client(), b);
    hub.complete(old.complete(DeliveryResult::Sent)).unwrap();
    assert!(hub.next(&mut Escaping).is_none());
    publish(&mut hub, c, Kind::Binary, b"new");
    let deliveries = [send(&mut hub), send(&mut hub)];
    assert_eq!(deliveries.map(|delivery| delivery.id.client()), [c, b]);
}

#[test]
fn none_callbacks_are_consumed_and_never_reissued() {
    struct Inline {
        sends: Vec<Delivery>,
    }
    impl Ports for Inline {
        type Output = ();
        fn send(&mut self, delivery: Delivery) -> Option<()> {
            self.sends.push(delivery);
            None
        }
        fn close(&mut self, _: Close) -> Option<()> {
            None
        }
        fn yield_turn(&mut self) -> Option<()> {
            None
        }
    }
    let mut hub = Hub::new(1, config()).unwrap();
    let a = add(&mut hub);
    add(&mut hub);
    publish(&mut hub, a, Kind::Text, b"inline");
    let mut ports = Inline { sends: Vec::new() };
    assert_eq!(hub.next(&mut ports), None);
    assert_eq!(ports.sends.len(), 2);
    assert_eq!(hub.next(&mut ports), None);
    assert_eq!(ports.sends.len(), 2);
    for delivery in ports.sends {
        hub.complete(delivery.complete(DeliveryResult::Sent))
            .unwrap();
    }
}

#[test]
fn config_rejects_overflow_and_inconsistent_minima() {
    let mut options = config();
    options.max_messages_per_client = usize::MAX;
    assert!(matches!(Hub::new(1, options), Err(Error::InvalidConfig)));
    let mut options = config();
    options.max_total_bytes = 1;
    assert!(matches!(Hub::new(1, options), Err(Error::InvalidConfig)));
    let mut options = config();
    options.max_client_bytes = options.max_message_bytes;
    assert!(matches!(Hub::new(1, options), Err(Error::InvalidConfig)));
}

#[test]
fn failed_delivery_removes_only_its_recipient() {
    let mut hub = Hub::new(1, config()).unwrap();
    let a = add(&mut hub);
    let b = add(&mut hub);
    publish(&mut hub, a, Kind::Text, b"broadcast");
    let failed = send(&mut hub);
    hub.complete(failed.complete(DeliveryResult::Failed))
        .unwrap();
    let valid = send(&mut hub);
    assert_eq!(valid.id.client(), b);
    hub.complete(valid.complete(DeliveryResult::Sent)).unwrap();
    assert_eq!(
        close(&mut hub),
        Close {
            client: a,
            code: 1011
        }
    );
    assert_eq!(hub.stats().active_clients, 1);
}

#[test]
fn issued_delivery_rejected_after_recipient_closes_does_not_close_hub() {
    let mut hub = Hub::new(1, config()).unwrap();
    let sender = add(&mut hub);
    let closing = add(&mut hub);
    let healthy = add(&mut hub);
    publish(&mut hub, sender, Kind::Text, b"pending");
    let sender_delivery = send(&mut hub);
    let pending_admission = send(&mut hub);
    let healthy_delivery = send(&mut hub);
    assert_eq!(pending_admission.id.client(), closing);
    // The root owns this send, but the recipient closes before protocol
    // admission. Earlier readiness is not a promise of future admission.
    hub.remove(closing, 1008).unwrap();
    assert_eq!(
        close(&mut hub),
        Close {
            client: closing,
            code: 1008
        }
    );
    hub.complete(pending_admission.complete(DeliveryResult::Failed))
        .unwrap();
    assert_eq!(hub.stats().resident_clients, 3);
    hub.closed(closing).unwrap();
    hub.complete(sender_delivery.complete(DeliveryResult::Sent))
        .unwrap();
    hub.complete(healthy_delivery.complete(DeliveryResult::Sent))
        .unwrap();
    let published = publish(&mut hub, healthy, Kind::Text, b"still open");
    assert_eq!((published.recipients, published.removed), (2, 0));
    let a = send(&mut hub);
    let b = send(&mut hub);
    assert_eq!([a.id.client(), b.id.client()], [sender, healthy]);
    for delivery in [a, b] {
        assert_eq!(delivery.payload.as_ref(), b"still open");
        hub.complete(delivery.complete(DeliveryResult::Sent))
            .unwrap();
    }
    assert!(hub.next(&mut Escaping).is_none());
}
