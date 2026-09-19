//! Bounded, synchronous broadcast application. No WebSocket or runtime policy.
use std::cell::Cell;
use std::mem::size_of;
use std::rc::Rc;

const RC_COUNTS: usize = 2 * size_of::<usize>();

#[derive(Clone, Debug)]
pub struct Config {
    pub max_clients: usize,
    pub max_message_bytes: usize,
    pub max_messages_per_client: usize,
    pub max_client_bytes: usize,
    pub max_total_bytes: usize,
    /// Root-owned buffers reserved per admitted client. The root calls `closed`
    /// only after these allocations and their original I/O have settled.
    pub external_bytes_per_client: usize,
    pub external_fixed_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_clients: 64,
            max_message_bytes: 1024 * 1024,
            max_messages_per_client: 32,
            max_client_bytes: 4 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            external_bytes_per_client: 0,
            external_fixed_bytes: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientId {
    owner: u64,
    slot: usize,
    generation: u64,
}

impl ClientId {
    pub fn slot(self) -> usize {
        self.slot
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Text,
    Binary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfig,
    AdmissionLimit,
    AggregateLimit,
    MessageLimit,
    InvalidText,
    UnknownClient,
    Closing,
    InvalidState,
    IdentityExhausted,
}

#[derive(Debug)]
struct Budget {
    limit: usize,
    used: Cell<usize>,
    peak: Cell<usize>,
}

impl Budget {
    fn reserve(self: &Rc<Self>, bytes: usize) -> Result<Reservation, Error> {
        let total = self
            .used
            .get()
            .checked_add(bytes)
            .ok_or(Error::AggregateLimit)?;
        if total > self.limit {
            return Err(Error::AggregateLimit);
        }
        self.used.set(total);
        self.peak.set(self.peak.get().max(total));
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

#[derive(Debug)]
struct Reservation {
    budget: Rc<Budget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.set(self.budget.used.get() - self.bytes);
    }
}

#[derive(Debug)]
struct Payload {
    storage: Box<[u8]>,
    length: usize,
    order: u64,
    kind: Kind,
    _capacity: Reservation,
    _metadata: Reservation,
}

/// Immutable outgoing storage. Sharing is private to the hub: a recipient
/// cannot clone a lease and return a receipt while retaining another copy.
#[derive(Debug)]
pub struct SharedPayload(Storage);

#[derive(Debug)]
enum Storage {
    Empty {
        order: u64,
        kind: Kind,
        budget: Rc<Budget>,
    },
    Data(Rc<Payload>),
}

impl SharedPayload {
    pub fn order(&self) -> u64 {
        match &self.0 {
            Storage::Empty { order, .. } => *order,
            Storage::Data(data) => data.order,
        }
    }
    pub fn kind(&self) -> Kind {
        match &self.0 {
            Storage::Empty { kind, .. } => *kind,
            Storage::Data(data) => data.kind,
        }
    }
    pub fn capacity(&self) -> usize {
        match &self.0 {
            Storage::Empty { .. } => 0,
            Storage::Data(data) => data.storage.len(),
        }
    }
    fn share(&self) -> Self {
        Self(match &self.0 {
            Storage::Empty {
                order,
                kind,
                budget,
            } => Storage::Empty {
                order: *order,
                kind: *kind,
                budget: budget.clone(),
            },
            Storage::Data(data) => Storage::Data(data.clone()),
        })
    }
    fn belongs_to(&self, budget: &Rc<Budget>) -> bool {
        let owned = match &self.0 {
            Storage::Empty { budget, .. } => budget,
            Storage::Data(data) => &data._capacity.budget,
        };
        Rc::ptr_eq(owned, budget)
    }
}

impl AsRef<[u8]> for SharedPayload {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Storage::Empty { .. } => &[],
            Storage::Data(data) => &data.storage[..data.length],
        }
    }
}

struct Assembly {
    kind: Kind,
    storage: Box<[u8]>,
    length: usize,
    charge: Reservation,
}

impl Assembly {
    fn append(&mut self, data: &[u8], maximum: usize) -> Result<(), Error> {
        let needed = self
            .length
            .checked_add(data.len())
            .filter(|n| *n <= maximum)
            .ok_or(Error::MessageLimit)?;
        if needed > self.storage.len() {
            let capacity = needed
                .checked_next_power_of_two()
                .unwrap_or(maximum)
                .min(maximum);
            // Reserve the entire new allocation before retaining old+new
            // storage together. Neither allocator rounding nor Vec growth is
            // used as an implicit, uncharged capacity expansion.
            let charge = self.charge.budget.reserve(capacity)?;
            let mut storage = zeroed(capacity);
            storage[..self.length].copy_from_slice(&self.storage[..self.length]);
            self.storage = storage;
            self.charge = charge;
        }
        self.storage[self.length..needed].copy_from_slice(data);
        self.length = needed;
        Ok(())
    }
}

struct Queue {
    entries: Box<[Option<SharedPayload>]>,
    head: usize,
    length: usize,
}

impl Queue {
    fn push(&mut self, payload: SharedPayload) {
        assert!(self.length < self.entries.len());
        let index = (self.head + self.length) % self.entries.len();
        self.entries[index] = Some(payload);
        self.length += 1;
    }
    fn pop(&mut self) -> Option<SharedPayload> {
        if self.length == 0 {
            return None;
        }
        let payload = self.entries[self.head].take();
        self.head = (self.head + 1) % self.entries.len();
        self.length -= 1;
        payload
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryId {
    client: ClientId,
    order: u64,
}

impl DeliveryId {
    pub fn client(self) -> ClientId {
        self.client
    }
}

#[derive(Debug)]
pub struct Delivery {
    pub id: DeliveryId,
    pub payload: SharedPayload,
}

impl Delivery {
    pub fn complete(self, result: DeliveryResult) -> DeliveryCompletion {
        DeliveryCompletion {
            id: self.id,
            payload: self.payload,
            result,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryResult {
    Sent,
    Failed,
}

#[derive(Debug)]
pub struct DeliveryCompletion {
    pub id: DeliveryId,
    pub payload: SharedPayload,
    pub result: DeliveryResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Close {
    pub client: ClientId,
    pub code: u16,
}

pub trait Ports {
    type Output;
    fn send(&mut self, delivery: Delivery) -> Option<Self::Output>;
    fn close(&mut self, close: Close) -> Option<Self::Output>;
    fn yield_turn(&mut self) -> Option<Self::Output>;
}

struct InFlight {
    id: DeliveryId,
    capacity: usize,
}

struct Client {
    id: ClientId,
    activated: bool,
    queue: Queue,
    assembly: Option<Assembly>,
    in_flight: Option<InFlight>,
    held_bytes: usize,
    close: Option<u16>,
    close_issued: bool,
    closed: bool,
    queued: bool,
    previous: Option<usize>,
    next: Option<usize>,
    _queue_storage: Reservation,
    _external_storage: Reservation,
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub active_clients: usize,
    pub resident_clients: usize,
    pub used_bytes: usize,
    pub peak_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Published {
    pub order: u64,
    pub recipients: usize,
    pub removed: usize,
}

/// One hub order governs every recipient queue. Client slots remain occupied
/// after logical closure until external storage and the outgoing lease settle.
pub struct Hub {
    owner: u64,
    config: Config,
    budget: Rc<Budget>,
    clients: Box<[Option<Client>]>,
    ready_head: Option<usize>,
    ready_tail: Option<usize>,
    next_client: u64,
    next_order: u64,
    active: usize,
    resident: usize,
    queue_bytes: usize,
    _slots: Reservation,
}

impl Hub {
    pub fn new(owner: u64, config: Config) -> Result<Self, Error> {
        let slots = config
            .max_clients
            .checked_mul(size_of::<Option<Client>>())
            .ok_or(Error::InvalidConfig)?;
        let queue_bytes = config
            .max_messages_per_client
            .checked_mul(size_of::<Option<SharedPayload>>())
            .and_then(|n| n.checked_add(size_of::<DeliveryCompletion>().max(size_of::<Delivery>())))
            .ok_or(Error::InvalidConfig)?;
        let ledger_bytes = (size_of::<Budget>() + RC_COUNTS + size_of::<Hub>())
            .checked_add(config.external_fixed_bytes)
            .ok_or(Error::InvalidConfig)?;
        let minimum = ledger_bytes
            .checked_add(slots)
            .and_then(|n| n.checked_add(queue_bytes))
            .and_then(|n| n.checked_add(config.external_bytes_per_client))
            .ok_or(Error::InvalidConfig)?;
        if config.max_clients == 0
            || config.max_clients > 4096
            || config.max_messages_per_client == 0
            || config.max_message_bytes == 0
            || config.max_total_bytes > isize::MAX as usize
            || queue_bytes
                .checked_add(config.max_message_bytes)
                .is_none_or(|n| n > config.max_client_bytes)
            || minimum > config.max_total_bytes
        {
            return Err(Error::InvalidConfig);
        }
        let budget = Rc::new(Budget {
            limit: config.max_total_bytes,
            used: Cell::new(ledger_bytes),
            peak: Cell::new(ledger_bytes),
        });
        let charge = budget.reserve(slots)?;
        let clients = empty_slots(config.max_clients);
        Ok(Self {
            owner,
            config,
            budget,
            clients,
            ready_head: None,
            ready_tail: None,
            next_client: 1,
            next_order: 1,
            active: 0,
            resident: 0,
            queue_bytes,
            _slots: charge,
        })
    }

    pub fn stats(&self) -> Stats {
        Stats {
            active_clients: self.active,
            resident_clients: self.resident,
            used_bytes: self.budget.used.get(),
            peak_bytes: self.budget.peak.get(),
        }
    }

    pub fn can_admit(&self) -> bool {
        self.resident < self.config.max_clients
            && self
                .budget
                .used
                .get()
                .checked_add(self.queue_bytes)
                .and_then(|n| n.checked_add(self.config.external_bytes_per_client))
                .is_some_and(|n| n <= self.budget.limit)
    }

    pub fn admit(&mut self) -> Result<ClientId, Error> {
        let slot = self
            .clients
            .iter()
            .position(Option::is_none)
            .ok_or(Error::AdmissionLimit)?;
        let next = self
            .next_client
            .checked_add(1)
            .ok_or(Error::IdentityExhausted)?;
        let queue_storage = self.budget.reserve(self.queue_bytes)?;
        let external_storage = self.budget.reserve(self.config.external_bytes_per_client)?;
        let id = ClientId {
            owner: self.owner,
            slot,
            generation: self.next_client,
        };
        self.next_client = next;
        self.clients[slot] = Some(Client {
            id,
            activated: false,
            queue: Queue {
                entries: empty_slots(self.config.max_messages_per_client),
                head: 0,
                length: 0,
            },
            assembly: None,
            in_flight: None,
            held_bytes: self.queue_bytes,
            close: None,
            close_issued: false,
            closed: false,
            queued: false,
            previous: None,
            next: None,
            _queue_storage: queue_storage,
            _external_storage: external_storage,
        });
        self.resident += 1;
        Ok(id)
    }

    /// Admission reserves storage; activation follows the completed HTTP
    /// upgrade. Pending handshakes are never broadcast recipients.
    pub fn activate(&mut self, id: ClientId) -> Result<(), Error> {
        let client = self.client_mut(id)?;
        if client.close.is_some() {
            return Err(Error::Closing);
        }
        if client.activated {
            return Err(Error::InvalidState);
        }
        client.activated = true;
        self.active += 1;
        Ok(())
    }

    fn client(&self, id: ClientId) -> Result<&Client, Error> {
        self.clients
            .get(id.slot)
            .and_then(Option::as_ref)
            .filter(|client| client.id == id)
            .ok_or(Error::UnknownClient)
    }
    fn client_mut(&mut self, id: ClientId) -> Result<&mut Client, Error> {
        self.clients
            .get_mut(id.slot)
            .and_then(Option::as_mut)
            .filter(|client| client.id == id)
            .ok_or(Error::UnknownClient)
    }
    fn open(&self, id: ClientId) -> Result<(), Error> {
        let client = self.client(id)?;
        if client.close.is_some() {
            Err(Error::Closing)
        } else if !client.activated {
            Err(Error::InvalidState)
        } else {
            Ok(())
        }
    }

    pub fn begin(&mut self, id: ClientId, kind: Kind) -> Result<(), Error> {
        self.open(id)?;
        if self.client(id)?.assembly.is_some() {
            return Err(Error::InvalidState);
        }
        let charge = self.budget.reserve(0)?;
        self.client_mut(id)?.assembly = Some(Assembly {
            kind,
            storage: Box::new([]),
            length: 0,
            charge,
        });
        Ok(())
    }

    pub fn append(&mut self, id: ClientId, bytes: &[u8]) -> Result<(), Error> {
        self.open(id)?;
        let maximum = self.config.max_message_bytes;
        let result = self
            .client_mut(id)?
            .assembly
            .as_mut()
            .ok_or(Error::InvalidState)?
            .append(bytes, maximum);
        if result.is_err() {
            self.remove(id, 1008)?;
        }
        result
    }

    pub fn finish(&mut self, id: ClientId) -> Result<Published, Error> {
        self.open(id)?;
        let assembly = self
            .client_mut(id)?
            .assembly
            .take()
            .ok_or(Error::InvalidState)?;
        if assembly.kind == Kind::Text
            && std::str::from_utf8(&assembly.storage[..assembly.length]).is_err()
        {
            self.remove(id, 1007)?;
            return Err(Error::InvalidText);
        }
        let Some(next_order) = self.next_order.checked_add(1) else {
            self.remove(id, 1008)?;
            return Err(Error::IdentityExhausted);
        };
        let order = self.next_order;
        let payload = if assembly.length == 0 {
            SharedPayload(Storage::Empty {
                order,
                kind: assembly.kind,
                budget: self.budget.clone(),
            })
        } else {
            let metadata = match self.budget.reserve(size_of::<Payload>() + RC_COUNTS) {
                Ok(charge) => charge,
                Err(error) => {
                    self.remove(id, 1008)?;
                    return Err(error);
                }
            };
            SharedPayload(Storage::Data(Rc::new(Payload {
                storage: assembly.storage,
                length: assembly.length,
                order,
                kind: assembly.kind,
                _capacity: assembly.charge,
                _metadata: metadata,
            })))
        };
        self.next_order = next_order;
        let mut report = Published {
            order,
            recipients: 0,
            removed: 0,
        };
        for slot in 0..self.clients.len() {
            let Some(client) = self.clients[slot]
                .as_ref()
                .filter(|client| client.activated && client.close.is_none())
            else {
                continue;
            };
            let id = client.id;
            let count = client.queue.length + usize::from(client.in_flight.is_some());
            let fits = count < self.config.max_messages_per_client
                && client
                    .held_bytes
                    .checked_add(payload.capacity())
                    .is_some_and(|bytes| bytes <= self.config.max_client_bytes);
            if fits {
                let client = self.client_mut(id)?;
                client.held_bytes += payload.capacity();
                client.queue.push(payload.share());
                self.mark_ready(slot);
                report.recipients += 1;
            } else {
                self.remove(id, 1008)?;
                report.removed += 1;
            }
        }
        Ok(report)
    }

    /// Logical removal does not release an in-flight recipient reservation.
    pub fn remove(&mut self, id: ClientId, code: u16) -> Result<(), Error> {
        let client = self.client_mut(id)?;
        if client.close.is_some() {
            return Ok(());
        }
        client.close = Some(code);
        client.assembly = None;
        while let Some(payload) = client.queue.pop() {
            client.held_bytes -= payload.capacity();
        }
        if client.activated {
            self.active -= 1;
        }
        self.mark_ready(id.slot);
        Ok(())
    }

    /// The root calls this after its original operations and external buffers
    /// settle, not merely when a protocol begins its closing handshake.
    pub fn closed(&mut self, id: ClientId) -> Result<(), Error> {
        self.remove(id, 1000)?;
        let client = self.client_mut(id)?;
        client.closed = true;
        client.close_issued = true;
        self.reap(id);
        Ok(())
    }

    pub fn complete(&mut self, completion: DeliveryCompletion) -> Result<(), DeliveryCompletion> {
        let valid = completion.payload.belongs_to(&self.budget)
            && self
                .client(completion.id.client)
                .ok()
                .and_then(|client| client.in_flight.as_ref())
                .is_some_and(|pending| {
                    pending.id == completion.id && completion.payload.order() == pending.id.order
                });
        if !valid {
            return Err(completion);
        }
        let id = completion.id.client;
        let client = self.client_mut(id).unwrap();
        let pending = client.in_flight.take().unwrap();
        client.held_bytes -= pending.capacity;
        if completion.result == DeliveryResult::Failed {
            self.remove(id, 1011).unwrap();
        }
        drop(completion);
        self.reap(id);
        if self.client(id).is_ok() {
            self.mark_ready(id.slot);
        }
        Ok(())
    }

    fn mark_ready(&mut self, slot: usize) {
        let client = self.clients[slot].as_mut().unwrap();
        if client.queued {
            return;
        }
        client.queued = true;
        client.previous = self.ready_tail;
        client.next = None;
        if let Some(tail) = self.ready_tail {
            self.clients[tail].as_mut().unwrap().next = Some(slot);
        } else {
            self.ready_head = Some(slot);
        }
        self.ready_tail = Some(slot);
    }

    fn unlink(&mut self, slot: usize) {
        let client = self.clients[slot].as_mut().unwrap();
        if !client.queued {
            return;
        }
        let previous = client.previous.take();
        let next = client.next.take();
        client.queued = false;
        if let Some(previous) = previous {
            self.clients[previous].as_mut().unwrap().next = next;
        } else {
            self.ready_head = next;
        }
        if let Some(next) = next {
            self.clients[next].as_mut().unwrap().previous = previous;
        } else {
            self.ready_tail = previous;
        }
    }

    fn reap(&mut self, id: ClientId) {
        if self
            .client(id)
            .is_ok_and(|client| client.closed && client.in_flight.is_none())
        {
            self.unlink(id.slot);
            self.clients[id.slot] = None;
            self.resident -= 1;
        }
    }

    pub fn next<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            for _ in 0..64 {
                let slot = self.ready_head?;
                self.unlink(slot);
                let client = self.clients[slot].as_mut().unwrap();
                let output = if let Some(code) = client.close {
                    if client.close_issued {
                        continue;
                    }
                    client.close_issued = true;
                    ports.close(Close {
                        client: client.id,
                        code,
                    })
                } else if client.in_flight.is_none() {
                    let Some(payload) = client.queue.pop() else {
                        continue;
                    };
                    let id = DeliveryId {
                        client: client.id,
                        order: payload.order(),
                    };
                    client.in_flight = Some(InFlight {
                        id,
                        capacity: payload.capacity(),
                    });
                    ports.send(Delivery { id, payload })
                } else {
                    continue;
                };
                if output.is_some() {
                    return output;
                }
            }
            self.ready_head?;
            if let Some(output) = ports.yield_turn() {
                return Some(output);
            }
        }
    }
}

pub(crate) fn empty_slots<T>(length: usize) -> Box<[Option<T>]> {
    let mut storage = Box::<[Option<T>]>::new_uninit_slice(length);
    for item in &mut storage {
        item.write(None);
    }
    // Every element now contains the initialized, non-owning None value.
    unsafe { storage.assume_init() }
}

fn zeroed(length: usize) -> Box<[u8]> {
    let mut storage = Box::<[u8]>::new_uninit_slice(length);
    for item in &mut storage {
        item.write(0);
    }
    // All bytes are initialized, and the boxed slice has exact capacity.
    unsafe { storage.assume_init() }
}

#[cfg(test)]
mod tests;
