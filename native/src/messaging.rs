//! The messaging vocabulary — pure, Zend-free, unit-testable.
//!
//! This is the Rust twin of the PHP SDK's `Service/MessagingDestination.php`,
//! `Service/MessagingBody.php` and `Service/MessagingWait.php`, and those files are
//! the REFERENCE: the two halves of a stream (a Go producer, a PHP consumer; a
//! bridge-instrumented publisher, a natively-observed one) join in the desktop on
//! these exact keys and these exact rules, so any drift here splits a stream's
//! producer graph in two. Every rule below cites the PHP original it mirrors, and
//! the decisive test cases at the bottom are ports of
//! `api/tests/bunny-case.php:428-457`.
//!
//! Nothing in this module touches `zend_*` types or thread-local request state —
//! the observer hands in strings and bytes it already read, and gets attributes
//! back. That is what lets `cargo test` prove the vocabulary without a PHP host
//! process (this crate's Zend-linked tests cannot RUN outside one; these can).

/// `messaging.system` for AMQP — the broker, never the client library: `bunny`
/// would describe how THIS process speaks AMQP, where `rabbitmq` is what the
/// other side of the stream is, and a Go producer on the same queue says
/// `rabbitmq` too, which is what lets the two join.
pub const SYSTEM_RABBITMQ: &str = "rabbitmq";

/// `messaging.system` for Kafka — the broker, never the client library:
/// `rdkafka` (or `php-rdkafka`) names the extension speaking the wire
/// protocol, `kafka` is what the other side of the stream is, and a
/// non-PHP producer on the same topic says `kafka` too — the fact that lets
/// the two join. See the RdKafka observation contract's "Vocabulary mapping"
/// section.
pub const SYSTEM_KAFKA: &str = "kafka";

/// `messaging.system` for NATS (core and JetStream alike — JetStream is a
/// layer over the same NATS server, not a different broker) — never
/// `"basis-nats"`, the client library. See the NATS observation contract §3.
pub const SYSTEM_NATS: &str = "nats";

/// The wall-clock instant of the publish, as an application header. Deliberately
/// NOT the AMQP `timestamp` property: that is a reserved, second-granularity
/// field the application may want for its own meaning, and a queue wait needs
/// sub-second resolution. Same header the PHP bridge
/// (`BunnyTelemetry::ENQUEUED_AT_HEADER`) and Laravel's `QueueTelemetry` stamp,
/// so either side can measure a wait whoever produced the message.
pub const ENQUEUED_AT_HEADER: &str = "x-chronos-enqueued-at";

/// THE GAP's fix: the publish side's own banked class name, carried on the
/// wire so a CONSUME span — which has no `serializeToString` call of its own
/// to bank against — can still resolve `messaging.message.name` to something
/// better than raw base64. Injected only when the publish side actually
/// resolved a name: the app's own `type` property, or failing that an
/// UNAMBIGUOUS banked class (see `bank_serialized_message`'s poison rule);
/// absent otherwise — this header is never a guess, so a consumer that reads
/// it is reading the publisher's own identity for the message, not a native
/// inference. Add-if-absent like every other
/// injected header: an application's own value under this key, if it ever set
/// one, wins.
pub const SCHEMA_HEADER: &str = "x-chronos-schema";

/// Preview ceiling for a PUBLISH body: it rides a span attribute, whose ceiling
/// is the PHP SDK's `Span::MAX_TEXT_LENGTH` (16384) — the same exemption
/// `db.statement` gets.
pub const PUBLISH_PREVIEW_CAP: usize = 16384;

/// Preview ceiling for a CONSUME body: it rides the request-attribute bag, whose
/// native cap is `request_attributes::MAX_VALUE_BYTES` (8192). The truncation
/// happens HERE, where `.truncated` can be set honestly — left to the bag's own
/// cap, an oversized body would arrive looking complete.
pub const CONSUME_PREVIEW_CAP: usize = 8192;

/// WHERE a message went, in the four normalised facts of
/// `MessagingDestination` (ADR 0024 §2): the leaf a consumer reads (`name`),
/// what scopes it (`namespace` — the vhost, for AMQP), what routed it there
/// (`via` — the exchange) and the selector used against it (`route` — the
/// routing key). Every field is `None` when it cannot be read: absent, never
/// guessed — a namespace inferred from a default ("probably `/`") would make the
/// desktop's join confident and wrong, where an absent one degrades to the
/// name-only match it already labels as ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub via: Option<String>,
    pub route: Option<String>,
}

impl Destination {
    /// The `messaging.destination.*` attributes, only the facts that could be
    /// read — a caller can extend its attribute list with this unconditionally.
    pub fn attributes(&self) -> Vec<(&'static str, String)> {
        let mut attributes = Vec::new();
        if let Some(name) = &self.name {
            attributes.push(("messaging.destination.name", name.clone()));
        }
        if let Some(namespace) = &self.namespace {
            attributes.push(("messaging.destination.namespace", namespace.clone()));
        }
        if let Some(via) = &self.via {
            attributes.push(("messaging.destination.via", via.clone()));
        }
        if let Some(route) = &self.route {
            attributes.push(("messaging.destination.route", route.clone()));
        }
        attributes
    }
}

/// AMQP, described from what the caller actually knows at the call — the exact
/// rules of `MessagingDestination::forAmqp`, restated:
///
///   * an explicitly known `queue` always wins the NAME. That is the consume
///     side, where the integration named the queue it subscribed to;
///   * with no known queue, the routing key becomes the NAME only for the
///     DEFAULT exchange, because making the routing key a queue name is
///     precisely what the nameless exchange does;
///   * a named exchange with no known queue leaves NAME ABSENT. Filling it with
///     the routing key would claim a queue that may not exist, and the desktop's
///     producer graph would attach the trace to a stream confidently and wrongly;
///   * VIA is omitted for the default exchange rather than filled with
///     `amq.default` — the nameless exchange cannot be named.
pub fn amqp_destination(vhost: &str, exchange: &str, routing_key: &str, queue: &str) -> Destination {
    let vhost = vhost.trim();
    let exchange = exchange.trim();
    let routing_key = routing_key.trim();
    let queue = queue.trim();

    let name = if !queue.is_empty() {
        Some(queue.to_owned())
    } else if exchange.is_empty() && !routing_key.is_empty() {
        Some(routing_key.to_owned())
    } else {
        None
    };
    let non_empty = |value: &str| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_owned())
        }
    };
    Destination {
        name,
        namespace: non_empty(vhost),
        via: non_empty(exchange),
        route: non_empty(routing_key),
    }
}

/// The publish span's name — `MessagingSpan::published`'s label chain, ported:
/// `PUBLISH <destination.name>`, else the exchange (`via` — the same string the
/// consumer's own binding names, so the label stays join-compatible), else the
/// routing key (only reachable on the default exchange, where the key IS the
/// queue), with the SYSTEM as the floor so nothing regresses to an empty name.
/// A routing key under a NAMED exchange is deliberately never folded in: in a
/// real estate those carry entity ids, and an unbounded span name makes a trace
/// list ungroupable. Never the broker when a place is known — "PUBLISH
/// oms.webhooks", never "PUBLISH rabbitmq".
pub fn publish_label(destination: &Destination) -> String {
    labeled("PUBLISH", destination, SYSTEM_RABBITMQ)
}

/// The generalised form of [`publish_label`]'s fallback chain — name, then
/// via, then route, then a broker floor — parameterised on the label PREFIX
/// and the floor word, so RdKafka's poll span, NATS's publish/poll/RPC spans
/// and any future broker can reuse the identical chain against their OWN
/// system word instead of RabbitMQ's. `publish_label` is kept as its own
/// function (rather than inlined at every RabbitMQ call site) because it is
/// already the tested, stable entry point Bunny's begin handler calls; this
/// is what it now delegates to.
pub fn labeled(prefix: &str, destination: &Destination, floor: &str) -> String {
    let place = destination
        .name
        .as_deref()
        .or(destination.via.as_deref())
        .or(destination.route.as_deref())
        .unwrap_or(floor);
    format!("{prefix} {place}")
}

/// Kafka's destination vocabulary (RdKafka contract, "Vocabulary mapping"):
/// the topic is the NAME, and nothing else applies — Kafka has no
/// exchange/routing-key indirection, so `namespace`/`via`/`route` are always
/// absent (there is no vhost-equivalent visible to the client, and no
/// separate routing fact beyond the topic itself). An empty/whitespace topic
/// is absence, never a guessed name.
pub fn kafka_destination(topic: &str) -> Destination {
    let topic = topic.trim();
    Destination {
        name: if topic.is_empty() {
            None
        } else {
            Some(topic.to_owned())
        },
        namespace: None,
        via: None,
        route: None,
    }
}

/// `RD_KAFKA_PARTITION_UA` — "let the partitioner choose," never a real
/// partition. Kafka's own advisory partition number, for the informational
/// (non-identity, per the contract) `messaging.kafka.destination.partition`
/// attribute — `None` for the unassigned sentinel, so a native span never
/// reports "partition -1" as if a partitioner had chosen one.
pub const KAFKA_PARTITION_UNASSIGNED: i64 = -1;

pub fn kafka_partition_attribute(partition: i64) -> Option<String> {
    if partition == KAFKA_PARTITION_UNASSIGNED {
        None
    } else {
        Some(partition.to_string())
    }
}

/// NATS core pub/sub destination (NATS contract §3: "NATS: subject = name").
/// `namespace` (the account) is always absent — assigned server-side from
/// the client's JWT/creds, never visible to this client (see the contract's
/// §7 "impossible" list) — and `via`/`route` are always absent too: a
/// subject is simultaneously the address and the filter, so there is no
/// separate routing fact the way an AMQP exchange/routing-key pair has one.
pub fn nats_destination(subject: &str) -> Destination {
    let subject = subject.trim();
    Destination {
        name: if subject.is_empty() {
            None
        } else {
            Some(subject.to_owned())
        },
        namespace: None,
        via: None,
        route: None,
    }
}

/// NATS JetStream pull-poll destination (NATS contract §3, "poll span"
/// rule): NAME is honestly absent — a single fetch can return messages
/// spanning several subjects when the consumer's filter is a wildcard, so
/// there is no one "place" to report before the batch is inspected, the same
/// "named exchange, no known queue" honesty `amqp_destination` already
/// practises. `via` is the stream (always known — a `Consumer`'s stream is a
/// required constructor argument) and `route` is the durable consumer name,
/// which doubles as `messaging.consumer.group.name` at the call site.
pub fn nats_jetstream_poll_destination(stream: &str, consumer: &str) -> Destination {
    let stream = stream.trim();
    let consumer = consumer.trim();
    Destination {
        name: None,
        namespace: None,
        via: if stream.is_empty() {
            None
        } else {
            Some(stream.to_owned())
        },
        route: if consumer.is_empty() {
            None
        } else {
            Some(consumer.to_owned())
        },
    }
}

/// Parse a JetStream pull-consumer's own request subject
/// (`$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>`, `Consumer::getQueue()`'s
/// `$launcher->subject`) into `(stream, consumer)`. NATS subject-token
/// grammar forbids `.` inside a single token, so splitting on the fixed
/// prefix and taking the two remaining tokens is EXACT, never a heuristic
/// guess (NATS contract §5(d)). `None` when the subject does not match this
/// exact shape at all (a future JetStream API subject this client doesn't
/// use, or a caller error) — absent, not a guess at the wrong two tokens.
pub fn jetstream_next_subject_stream_and_consumer(subject: &str) -> Option<(String, String)> {
    let suffix = subject.strip_prefix("$JS.API.CONSUMER.MSG.NEXT.")?;
    let mut parts = suffix.splitn(2, '.');
    let stream = parts.next()?;
    let consumer = parts.next()?;
    if stream.is_empty() || consumer.is_empty() || consumer.contains('.') {
        // A well-formed subject has exactly two tokens left after the fixed
        // prefix; a further embedded '.' means this subject does not match
        // the shape this parser understands — absent, not a wrong guess.
        return None;
    }
    Some((stream.to_owned(), consumer.to_owned()))
}

/// `messaging.message.name` on the CONSUME side: THE GAP's fix. `schema_header`
/// is `x-chronos-schema` as read off the delivery's application headers (empty
/// when absent — the publish side only ever injects it for an unambiguous
/// banked class, never a guess); `type_header` is the AMQP `type` property.
///
/// The header wins over the property, deliberately the reverse of the usual
/// "application data beats telemetry" rule: `x-chronos-schema` is not a native
/// inference, it IS the publisher's own resolved class
/// (`begin_messaging_publish`'s `type`-property-or-serializeToString-bank
/// lookup, mirrored on this exact value), so it is strictly more precise than
/// a hand-set `type` string — an application can set `type` to a version tag
/// or routing hint that need not match the class actually serialized, where
/// the header is only ever present when it names that exact class. `type`
/// remains the fallback for every publish this header cannot reach: a
/// pre-upgrade estate, a non-PHP producer (a Go publisher never sets it),
/// or a hand-built AMQP client. Neither is ever back-filled from the queue
/// name — a queue is a PLACE, and naming it as the message type would make
/// every message on one queue look like one type.
pub fn resolve_message_name(schema_header: &str, type_header: &str) -> Option<String> {
    if !schema_header.is_empty() {
        Some(schema_header.to_owned())
    } else if !type_header.is_empty() {
        Some(type_header.to_owned())
    } else {
        None
    }
}

/// The payload's wire format as a bounded low-cardinality word — the port of
/// `BunnyTelemetry::protocol`. Only what the publisher DECLARED, never sniffed
/// from bytes; lowercased and trimmed before matching because a media type is
/// case-insensitive per RFC 9110 §8.3.1 and both halves of one stream have to
/// agree (`application/X-Protobuf` is legal and what several generators emit).
/// An unrecognised or absent content type yields `None` and the attribute is
/// dropped.
pub fn protocol(content_type: &str) -> Option<&'static str> {
    let media_type = content_type.trim().to_ascii_lowercase();
    if media_type.contains("protobuf") {
        return Some("protobuf");
    }
    if media_type.contains("json") {
        return Some("json");
    }
    None
}

/// One encoded body: the payload as it will ride an attribute, whether it went
/// out base64 (binary) or bare (text), and whether the cap cut it.
#[derive(Debug, PartialEq, Eq)]
pub struct EncodedBody {
    pub body: String,
    /// True when the payload was base64-encoded (it was not clean text). The
    /// span carries `messaging.message.body.encoding=base64` then and nothing
    /// otherwise — text is the default and stamping `utf-8` onto every JSON
    /// body would be noise on every span in the estate.
    pub base64: bool,
    pub truncated: bool,
}

/// Encode one payload into a byte budget — the port of `MessagingBody`'s
/// `text()`/`binary()` pair, over RAW BYTES rather than a `String` because the
/// readers must hand bytes across (a lossy UTF-8 conversion would corrupt a
/// protobuf payload before this decision ever ran).
///
/// Text (valid UTF-8, no C0 controls beyond tab/LF/CR) is cut to the budget on
/// a char boundary. Binary is cut FIRST to the largest whole-triplet length
/// whose encoding fits (`(cap / 4) * 3`) and only then base64'd — encoding
/// first and cutting after would overshoot the ceiling by 1.33x and could sever
/// a 4-character base64 group, producing a value that no longer decodes; cut
/// first, the emitted attribute lands at or under the cap and decodes cleanly
/// to a genuine prefix of the payload.
///
/// `None` for an empty payload or a zero budget — an absent attribute, never an
/// empty one. The capture GATE is the caller's job ([`capture_enabled`]), so
/// this stays a pure function of its arguments.
pub fn encode_body(bytes: &[u8], cap: usize) -> Option<EncodedBody> {
    if bytes.is_empty() || cap == 0 {
        return None;
    }
    if let Some(text) = as_text(bytes) {
        let cut = floor_char_boundary(text, cap.min(text.len()));
        return Some(EncodedBody {
            body: text[..cut].to_owned(),
            base64: false,
            truncated: cut < bytes.len(),
        });
    }
    let budget = (cap / 4) * 3;
    if budget == 0 {
        return None;
    }
    let raw = &bytes[..budget.min(bytes.len())];
    Some(EncodedBody {
        body: base64_encode(raw),
        base64: true,
        truncated: raw.len() < bytes.len(),
    })
}

/// The WHOLE payload for the span-body store — the port of
/// `MessagingBody::whole`. Same gate, same text test, same cut-before-encode
/// rule as [`encode_body`], cut only to the operator's `budget` — and produced
/// ONLY when that budget genuinely exceeds the preview ceiling the span
/// attribute was already cut to (mirroring `http_capture`'s own rule): a blob
/// identical to the attribute beside it costs a NATS message, a hypertable row
/// and a round trip to say what the span already said.
///
/// Returns `(payload, transfer_encoding)` where the encoding is `"base64"` or
/// `""` — the shape `chronos_store_span_body` accepts. The encoding a READER
/// consults is the span's `.encoding` attribute, set by [`encode_body`] from
/// the identical test: one fact, not two that can drift.
pub fn whole_body(bytes: &[u8], preview_ceiling: usize, budget: usize) -> Option<(String, &'static str)> {
    if budget == 0 || budget <= preview_ceiling {
        return None;
    }
    let encoded = encode_body(bytes, budget)?;
    Some((encoded.body, if encoded.base64 { "base64" } else { "" }))
}

/// How long the message waited, in milliseconds — the port of
/// `MessagingWait::milliseconds`, clause for clause.
///
/// `None` rather than zero in every unknowable case: a missing or non-numeric
/// stamp, a non-finite or non-positive instant, or a stamp in the FUTURE
/// (clock skew). Zero is a measurement meaning "picked up instantly", and an
/// unmeasured queue must not report the healthiest possible value. A negative
/// reading is discarded whole instead of clamped — skew of a second in one
/// direction is skew of a second in the other, so the positive readings of a
/// skewed pair are wrong by as much as the negative ones; clamping only HIDES
/// it.
pub fn wait_milliseconds(enqueued_at: &str, started_at_seconds: f64) -> Option<u64> {
    let enqueued: f64 = enqueued_at.trim().parse().ok()?;
    if !enqueued.is_finite() || enqueued <= 0.0 {
        return None;
    }
    let waited = (started_at_seconds - enqueued) * 1000.0;
    if !waited.is_finite() || waited < 0.0 {
        return None;
    }
    Some(waited.round() as u64)
}

/// The enqueued-at stamp for the wire: epoch seconds at microsecond precision,
/// the exact `sprintf('%.6F', microtime(true))` shape `BunnyTelemetry` and
/// `QueueTelemetry` write, so either side's consumer parses either side's
/// producer.
pub fn enqueued_at_stamp(epoch_seconds: f64) -> String {
    format!("{epoch_seconds:.6}")
}

/// Whether captured message PAYLOADS are wanted at all. ON whenever the
/// collector is on; only an explicit `0|false|no|off` in
/// `CHRONOS_PHP_MESSAGING_CAPTURE_BODIES` disables — the spelling rule of
/// `NativeExtension::messagingCapturing()` (whose docblock, not
/// `MessagingBody.php`'s stale off-by-default header comment, is the
/// authority). Resolved once per process: the same `OnceLock` shape as every
/// other settings flag here, and with capture off not one byte of payload is
/// copied or encoded — only `strlen` survives, because size is the one payload
/// fact that is free.
pub fn capture_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        match crate::settings::get("CHRONOS_PHP_MESSAGING_CAPTURE_BODIES") {
            Some(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            None => true,
        }
    })
}

/// The most of one message body the operator has allowed, in bytes:
/// `CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY`, default 65536 (the Go SDK's own
/// default, so a Go producer and a PHP producer on one stream cap identically),
/// hard-clamped to 512 KiB — the same numbers as
/// `NativeExtension::messagingBodyCeiling()`. A ceiling on the operator's
/// number, not the effective limit: each call site min()s it against its own
/// preview bound.
pub fn body_ceiling() -> usize {
    static CEILING: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CEILING.get_or_init(|| {
        let configured = crate::settings::get("CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY")
            .and_then(|value| value.trim().parse::<i64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(65536);
        usize::try_from(configured).unwrap_or(65536).min(512 * 1024)
    })
}

/// Whether this payload can go out as text: valid UTF-8 with no C0 control
/// bytes other than tab, LF and CR — `MessagingBody::isText`'s two tests, both
/// needed because either alone is wrong (a protobuf message of small positive
/// varints is legal UTF-8 and is still binary; the control-byte test alone
/// would admit non-UTF-8 Latin-1). Tab/LF/CR are exempt because they appear in
/// perfectly ordinary pretty-printed JSON and XML.
fn as_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let control = bytes
        .iter()
        .any(|byte| *byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r'));
    if control {
        return None;
    }
    Some(text)
}

/// The largest char boundary at or below `at` — the same walk `lib.rs::cap`
/// does, local so a pure module needs nothing from the FFI half.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut end = at.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Standard base64 (RFC 4648, with padding) over raw bytes. Hand-rolled rather
/// than a new dependency: this crate is compiled inside every application
/// container against that container's PHP headers, and each crate added is a
/// build every estate image pays — twenty lines of table lookup is cheaper than
/// a supply chain entry.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Per-request ceiling on remembered serialized-message identities
/// ([`MessageBank`]). A request serialises few messages (the docblock on the
/// bank itself explains why), so 64 is a generous multiple of the realistic
/// count — a ring, not a budget anyone should ever feel.
pub const MESSAGE_BANK_CAPACITY: usize = 64;

/// One banked identity: the (hash, length) pair a publish body is looked up
/// by, and the class it resolved to — or `None` when a SECOND, differently
/// classed insert collided with an already-banked identity this request.
struct BankEntry {
    hash: u64,
    len: usize,
    /// `None` means POISONED: this exact (hash, len) has been seen under more
    /// than one class this request, so it is no longer safe to answer with
    /// either. Kept as an entry (rather than removed) so the collision stays
    /// remembered — a THIRD insert at the same identity must not un-poison it
    /// by chance agreement with one of the two it already disagreed with.
    class: Option<String>,
}

/// GAP 1's identity bank: `Google\Protobuf\Internal\Message::serializeToString`'s
/// end handler records `($this's real class, the returned bytes)` here; a
/// `MessagingPublish` begin handler with no AMQP `type` header looks its body
/// up by the SAME identity to recover `messaging.message.name`.
///
/// IDENTITY = (FNV-1a 64 hash of the bytes, byte length). Not a full digest
/// (sha256, already a dependency here for body-store content addressing)
/// because a serialized protobuf's OWN class + field set already carries most
/// of the collision resistance a request needs: two DIFFERENT DTOs landing on
/// the same 64-bit hash AND the same exact byte length, within the same
/// request, in the same short-lived bank, is the kind of coincidence a cheap
/// hash is allowed to risk — as long as risking it can only ever COST a name.
/// It always can: [`MessageBank::record`] POISONS (nulls) an identity the
/// moment a second, differently-classed insert touches it, and
/// [`MessageBank::lookup`] answers a poisoned identity with `None` — so the
/// worst outcome of a collision is a publish span with no
/// `messaging.message.name`, identical to what an application that sets no
/// `type` header already gets today. It can NEVER produce a WRONG class,
/// because the only way an identity ever carries a class at all is a single,
/// so-far-unambiguous insert.
///
/// Bounded to [`MESSAGE_BANK_CAPACITY`] entries as a ring (oldest evicted
/// first) — a request serialises few messages, so eviction is a backstop
/// against a pathological loop, not a limit real traffic brushes against.
pub struct MessageBank {
    entries: Vec<BankEntry>,
    capacity: usize,
}

impl MessageBank {
    pub fn new() -> Self {
        Self::with_capacity(MESSAGE_BANK_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        MessageBank {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record one serialized message's identity. A no-op for an empty class
    /// name or an empty payload: an empty protobuf message serializes to ZERO
    /// bytes regardless of class, so banking a zero-length identity would
    /// poison it on the very first request that serializes two different
    /// empty messages — punishing every other lookup this request for a case
    /// that carries no information to begin with.
    pub fn record(&mut self, class: &str, bytes: &[u8]) {
        if class.is_empty() || bytes.is_empty() {
            return;
        }
        let (hash, len) = identity(bytes);
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.hash == hash && entry.len == len)
        {
            if existing.class.as_deref() != Some(class) {
                // Two different classes, one identity: neither is trustworthy
                // any more. Poison rather than overwrite — overwriting would
                // silently prefer whichever class serialized second.
                existing.class = None;
            }
            return;
        }
        if self.entries.len() >= self.capacity {
            // Ring eviction: oldest first. A linear remove(0) is fine at this
            // capacity (a handful of entries, once per publish at most).
            self.entries.remove(0);
        }
        self.entries.push(BankEntry {
            hash,
            len,
            class: Some(class.to_owned()),
        });
    }

    /// The banked class for this exact body, if the identity was recorded
    /// exactly once this request. `None` for a body never seen (nothing was
    /// serialized through the observed method), a poisoned identity
    /// (ambiguous this request), or an empty body.
    pub fn lookup(&self, bytes: &[u8]) -> Option<&str> {
        if bytes.is_empty() {
            return None;
        }
        let (hash, len) = identity(bytes);
        self.entries
            .iter()
            .find(|entry| entry.hash == hash && entry.len == len)
            .and_then(|entry| entry.class.as_deref())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for MessageBank {
    fn default() -> Self {
        Self::new()
    }
}

fn identity(bytes: &[u8]) -> (u64, usize) {
    (fnv1a64(bytes), bytes.len())
}

/// FNV-1a, 64-bit. Hand-rolled for the same reason [`base64_encode`] is: this
/// crate compiles inside every application container, and a hashing crate
/// (even a tiny one) is a dependency every estate image would pay for. FNV-1a
/// is a handful of lines, has no cryptographic pretensions, and is exactly as
/// strong as [`MessageBank`] ever asks a hash to be — see its docblock for why
/// a weak-but-cheap hash is the correct choice here, not a compromise.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Apply propagation entries to a header map with ADD-IF-ABSENT semantics — the
/// pure statement of the injection rule the observer's zval writer enforces
/// per key with `zend_hash` lookups. A caller-supplied `traceparent` always
/// wins; ours is simply not added — instrumentation silently rewriting an
/// application's own propagation would be the worst kind of bug, because it
/// would look like a working trace. Same rule independently for `tracestate`,
/// `baggage` and the enqueued-at stamp. Native never validates or rewrites a
/// caller's value.
///
/// Exists as a testable function: the zval path cannot run under `cargo test`
/// (no PHP host process), so the MERGE rule is proven here and the zval writer
/// is a mechanical application of it.
pub fn add_if_absent(
    headers: &mut std::collections::BTreeMap<String, String>,
    entries: &[(&str, String)],
) {
    for (key, value) in entries {
        headers
            .entry((*key).to_owned())
            .or_insert_with(|| value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- The four decisive forAmqp cases (api/tests/bunny-case.php:428-457) ---

    #[test]
    fn a_topic_publish_carries_no_destination_name_and_names_via_plus_route() {
        // Zero, one or six queues may be bound to that routing key, and the
        // publisher cannot know which. Filling NAME here would attach the trace
        // to a stream confidently and wrongly.
        let destination = amqp_destination("oms", "organizations", "order.created", "");
        assert_eq!(destination.name, None, "a topic publish must carry no destination name");
        assert_eq!(destination.namespace.as_deref(), Some("oms"));
        assert_eq!(destination.via.as_deref(), Some("organizations"));
        assert_eq!(destination.route.as_deref(), Some("order.created"));
    }

    #[test]
    fn the_default_exchange_makes_the_routing_key_the_queue_and_omits_via() {
        // The nameless exchange is the one case where a routing key IS a queue
        // name — that is what it does. `via` stays absent rather than becoming
        // `amq.default`, because the nameless exchange has no name to record.
        let destination = amqp_destination("oms", "", "asn-items", "");
        assert_eq!(destination.name.as_deref(), Some("asn-items"));
        assert_eq!(
            destination.route.as_deref(),
            Some("asn-items"),
            "route is kept too — the duplication is how the default exchange routes"
        );
        assert_eq!(destination.via, None, "the default exchange must not be named");
    }

    #[test]
    fn an_explicitly_known_queue_wins_whatever_the_exchange_was() {
        let destination = amqp_destination("oms", "organizations", "order.created", "oms-orders");
        assert_eq!(destination.name.as_deref(), Some("oms-orders"));
        assert_eq!(destination.via.as_deref(), Some("organizations"));
    }

    #[test]
    fn an_unknown_vhost_is_absent_never_the_default_slash() {
        let destination = amqp_destination("", "organizations", "order.created", "");
        assert_eq!(
            destination.namespace, None,
            "an unknown vhost must be absent, never \"/\""
        );
    }

    #[test]
    fn identifier_ish_edge_inputs_are_trimmed_and_emptiness_is_absence() {
        let destination = amqp_destination("  ", " ex ", "  rk ", "\t");
        assert_eq!(destination.namespace, None);
        assert_eq!(destination.via.as_deref(), Some("ex"));
        assert_eq!(destination.route.as_deref(), Some("rk"));
        assert_eq!(destination.name, None, "a whitespace queue is no queue");
        // All-empty facts: every attribute absent, and the label falls to the floor.
        let empty = amqp_destination("", "", "", "");
        assert!(empty.attributes().is_empty());
        assert_eq!(publish_label(&empty), "PUBLISH rabbitmq");
    }

    // --- Span label fallback chain (MessagingSpan::published) ---------------

    #[test]
    fn the_publish_label_walks_queue_then_exchange_then_key_then_the_broker_floor() {
        // A known queue names the span.
        let queue = amqp_destination("shared", "oms.webhooks", "order.created", "admin_oms_orders");
        assert_eq!(publish_label(&queue), "PUBLISH admin_oms_orders");
        // A queueless topic publish is named after the exchange — the same
        // string the consumer's binding names — not after the broker.
        let topic = amqp_destination("shared", "oms.webhooks", "order.created", "");
        assert_eq!(publish_label(&topic), "PUBLISH oms.webhooks");
        // The default exchange: the routing key IS the queue, so it is the name
        // (via the NAME rule; the route fallback is unreachable ahead of it).
        let default_exchange = amqp_destination("shared", "", "order.created", "");
        assert_eq!(publish_label(&default_exchange), "PUBLISH order.created");
        // Nothing known at all: the system floor, never an empty label.
        let nothing = amqp_destination("", "", "", "");
        assert_eq!(publish_label(&nothing), "PUBLISH rabbitmq");
    }

    // --- messaging.message.name precedence (THE GAP's fix) -------------------

    #[test]
    fn the_schema_header_wins_over_the_type_property() {
        // The publisher's own banked class beats a hand-set `type` string —
        // the header is strictly more precise, never a guess.
        assert_eq!(
            resolve_message_name("QlsProtocol\\Shared\\Webhook", "some.other.tag"),
            Some("QlsProtocol\\Shared\\Webhook".to_owned())
        );
    }

    #[test]
    fn the_type_property_is_the_fallback_when_the_header_is_absent() {
        // Pre-upgrade estates and non-PHP producers never set the header.
        assert_eq!(
            resolve_message_name("", "QlsProtocol\\Shared\\Webhook"),
            Some("QlsProtocol\\Shared\\Webhook".to_owned())
        );
    }

    #[test]
    fn neither_source_present_names_nothing_rather_than_guessing() {
        assert_eq!(resolve_message_name("", ""), None);
    }

    // --- Protocol word -------------------------------------------------------

    #[test]
    fn the_protocol_word_is_declared_case_insensitive_and_never_sniffed() {
        assert_eq!(protocol("application/x-protobuf"), Some("protobuf"));
        // Legal per RFC 9110 §8.3.1, and what several generators emit; without
        // the lowercase both halves of one stream would spell one fact two ways.
        assert_eq!(protocol("application/X-Protobuf"), Some("protobuf"));
        assert_eq!(protocol(" application/json ; charset=utf-8"), Some("json"));
        assert_eq!(protocol("text/plain"), None);
        assert_eq!(protocol(""), None);
    }

    // --- Body encoding -------------------------------------------------------

    #[test]
    fn a_text_body_is_cut_on_a_char_boundary_and_marked_truncated() {
        let body = "héllo wörld".as_bytes();
        let encoded = encode_body(body, 2).expect("text encodes");
        // Byte 2 splits the 'é' — the cut walks back to byte 1.
        assert_eq!(encoded.body, "h");
        assert!(!encoded.base64);
        assert!(encoded.truncated);

        let whole = encode_body(body, 1024).expect("fits");
        assert_eq!(whole.body, "héllo wörld");
        assert!(!whole.truncated);
    }

    #[test]
    fn a_binary_body_is_cut_to_whole_triplets_so_the_prefix_still_decodes() {
        // A protobuf-ish payload: control bytes make it binary even though some
        // prefixes are valid UTF-8.
        let body: Vec<u8> = (0u8..=255).collect();
        let encoded = encode_body(&body, 16).expect("binary encodes");
        assert!(encoded.base64);
        assert!(encoded.truncated);
        // (16/4)*3 = 12 raw bytes → 16 encoded chars, no severed group.
        assert_eq!(encoded.body.len(), 16);
        assert_eq!(base64_encode(&body[..12]), encoded.body);
    }

    #[test]
    fn legal_utf8_with_control_bytes_is_still_binary() {
        // A protobuf message of small positive varints is valid UTF-8 — the
        // control-byte test is what keeps it off the text path.
        let body = b"\x08\x96\x01\x12\x03abc";
        let encoded = encode_body(body, 64).expect("encodes");
        assert!(encoded.base64, "control bytes force base64");
        // Tab/LF/CR stay text: they appear in ordinary pretty-printed JSON.
        let pretty = b"{\n\t\"k\": 1\r\n}";
        assert!(!encode_body(pretty, 64).expect("encodes").base64);
    }

    #[test]
    fn an_empty_body_and_a_zero_budget_encode_nothing() {
        assert_eq!(encode_body(b"", 1024), None);
        assert_eq!(encode_body(b"x", 0), None);
        // A budget under one base64 group encodes nothing rather than garbage.
        assert_eq!(encode_body(b"\x00\x01", 3), None);
    }

    #[test]
    fn the_whole_copy_exists_only_beyond_the_preview_cap() {
        let body = vec![b'a'; 100];
        // Operator allowance no larger than the preview: the blob would only
        // repeat the attribute, so nothing is stored.
        assert_eq!(whole_body(&body, 16384, 16384), None);
        assert_eq!(whole_body(&body, 16384, 1024), None);
        // A genuinely larger allowance stores the longer cut.
        let (payload, encoding) = whole_body(&vec![b'a'; 40_000], 16384, 65536).expect("stored");
        assert_eq!(payload.len(), 40_000);
        assert_eq!(encoding, "");
        // Binary reports its transfer encoding.
        let binary: Vec<u8> = vec![0u8; 40_000];
        let (_, encoding) = whole_body(&binary, 16384, 65536).expect("stored");
        assert_eq!(encoding, "base64");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 §10 test vectors — the decoder on the desktop is standard,
        // so the encoder must be too.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    // --- Queue wait ----------------------------------------------------------

    #[test]
    fn the_wait_is_none_never_zero_when_it_cannot_be_said_honestly() {
        let now = 1_757_500_000.0_f64;
        // Missing / garbage stamps: another producer's format, or none at all.
        assert_eq!(wait_milliseconds("", now), None);
        assert_eq!(wait_milliseconds("not-a-number", now), None);
        assert_eq!(wait_milliseconds("NaN", now), None);
        assert_eq!(wait_milliseconds("inf", now), None);
        assert_eq!(wait_milliseconds("0", now), None);
        assert_eq!(wait_milliseconds("-5", now), None);
        // A FUTURE stamp is clock skew, discarded whole rather than clamped —
        // clamping would hide that the positive readings are wrong by as much.
        assert_eq!(wait_milliseconds(&format!("{}", now + 1.0), now), None);
        // A real wait rounds to milliseconds.
        assert_eq!(wait_milliseconds(&format!("{:.6}", now - 1.5), now), Some(1500));
        assert_eq!(wait_milliseconds(&format!("{:.6}", now), now), Some(0));
    }

    #[test]
    fn the_enqueued_stamp_round_trips_through_the_wait_rule() {
        let sent = 1_757_500_000.123456_f64;
        let stamp = enqueued_at_stamp(sent);
        assert_eq!(stamp, "1757500000.123456");
        assert_eq!(wait_milliseconds(&stamp, sent + 0.25), Some(250));
    }

    // --- Injection merge -----------------------------------------------------

    #[test]
    fn a_callers_own_traceparent_always_wins_per_key() {
        use std::collections::BTreeMap;
        let mut headers: BTreeMap<String, String> = BTreeMap::from([
            ("traceparent".to_owned(), "00-caller-caller-01".to_owned()),
            ("content-type".to_owned(), "application/x-protobuf".to_owned()),
        ]);
        add_if_absent(
            &mut headers,
            &[
                ("traceparent", "00-ours-ours-01".to_owned()),
                ("tracestate", "chronos=1".to_owned()),
                (ENQUEUED_AT_HEADER, "1757500000.000001".to_owned()),
            ],
        );
        // The caller's promise stays on the wire untouched; ours is simply not
        // added — never validated, never rewritten.
        assert_eq!(headers["traceparent"], "00-caller-caller-01");
        // Absent keys land, each independently.
        assert_eq!(headers["tracestate"], "chronos=1");
        assert_eq!(headers[ENQUEUED_AT_HEADER], "1757500000.000001");
        // Application headers are untouched.
        assert_eq!(headers["content-type"], "application/x-protobuf");
    }

    #[test]
    fn enqueued_at_is_added_even_when_no_trace_context_exists() {
        use std::collections::BTreeMap;
        // A scheduled command's message has waited just as long — the stamp
        // rides every publish, traceparent or not.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        add_if_absent(&mut headers, &[(ENQUEUED_AT_HEADER, "1757500000.5".to_owned())]);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key(ENQUEUED_AT_HEADER));
    }

    // --- MessageBank (GAP 1: messaging.message.name via serializeToString) --

    #[test]
    fn a_unique_body_recovers_its_class_by_identity_alone() {
        let mut bank = MessageBank::new();
        bank.record("QlsProtocol\\Shared\\Webhook", b"\x08\x96\x01\x12\x03abc");
        assert_eq!(
            bank.lookup(b"\x08\x96\x01\x12\x03abc"),
            Some("QlsProtocol\\Shared\\Webhook")
        );
        // A body never recorded is a plain miss, not a guess.
        assert_eq!(bank.lookup(b"never serialized"), None);
    }

    #[test]
    fn recording_the_same_body_and_class_twice_stays_a_clean_match() {
        // Two messages of the same type in one request must not look like an
        // ambiguity to themselves.
        let mut bank = MessageBank::new();
        bank.record("QlsProtocol\\Shared\\Webhook", b"same bytes");
        bank.record("QlsProtocol\\Shared\\Webhook", b"same bytes");
        assert_eq!(bank.lookup(b"same bytes"), Some("QlsProtocol\\Shared\\Webhook"));
    }

    #[test]
    fn two_different_classes_sharing_one_identity_poison_it_to_no_name_ever() {
        // The load-bearing safety property: a collision must only ever COST a
        // name, never assign the WRONG one.
        let mut bank = MessageBank::new();
        bank.record("QlsProtocol\\Shared\\Webhook", b"colliding bytes");
        bank.record("QlsProtocol\\Shared\\OtherEvent", b"colliding bytes");
        assert_eq!(
            bank.lookup(b"colliding bytes"),
            None,
            "an ambiguous identity must resolve to no name, never either candidate"
        );
        // A THIRD insert agreeing with one of the two must not un-poison it —
        // the identity is untrustworthy for the rest of the request.
        bank.record("QlsProtocol\\Shared\\Webhook", b"colliding bytes");
        assert_eq!(bank.lookup(b"colliding bytes"), None);
    }

    #[test]
    fn an_empty_payload_is_never_banked_or_looked_up() {
        // An empty protobuf message serializes to zero bytes regardless of
        // class — banking that identity would poison it for every other
        // empty message this request, for a case with no information in it.
        let mut bank = MessageBank::new();
        bank.record("QlsProtocol\\Shared\\Webhook", b"");
        assert_eq!(bank.lookup(b""), None);
        assert_eq!(bank.lookup(b"anything"), None);
    }

    #[test]
    fn the_bank_is_a_ring_that_evicts_the_oldest_identity_first() {
        let mut bank = MessageBank::with_capacity(2);
        bank.record("First", b"one");
        bank.record("Second", b"two");
        bank.record("Third", b"three");
        // "one" was the oldest at capacity — evicted to make room for "three".
        assert_eq!(bank.lookup(b"one"), None);
        assert_eq!(bank.lookup(b"two"), Some("Second"));
        assert_eq!(bank.lookup(b"three"), Some("Third"));
    }

    #[test]
    fn fnv1a_matches_a_known_vector_and_is_sensitive_to_every_byte() {
        // FNV-1a 64-bit of the empty string is the offset basis unchanged.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        // Single-byte and near-identical inputs must not collide trivially —
        // the whole point of hashing rather than trusting length alone.
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
    }

    // --- Kafka destination + partition (RdKafka contract) --------------------

    #[test]
    fn kafka_destination_names_only_the_topic_never_a_namespace_or_route() {
        let destination = kafka_destination("orders.created");
        assert_eq!(destination.name.as_deref(), Some("orders.created"));
        assert_eq!(destination.namespace, None);
        assert_eq!(destination.via, None);
        assert_eq!(destination.route, None);
        // Whitespace-only is absence, same rule as amqp_destination.
        assert_eq!(kafka_destination("   ").name, None);
    }

    #[test]
    fn kafka_poll_label_uses_the_generic_chain_against_the_kafka_floor() {
        let named = kafka_destination("orders.created");
        assert_eq!(labeled("POLL", &named, SYSTEM_KAFKA), "POLL orders.created");
        let nothing = kafka_destination("");
        assert_eq!(labeled("POLL", &nothing, SYSTEM_KAFKA), "POLL kafka");
    }

    #[test]
    fn the_unassigned_partition_sentinel_is_absent_never_reported_as_negative_one() {
        assert_eq!(kafka_partition_attribute(-1), None);
        assert_eq!(kafka_partition_attribute(0), Some("0".to_owned()));
        assert_eq!(kafka_partition_attribute(7), Some("7".to_owned()));
    }

    // --- NATS destinations (NATS contract §3) ---------------------------------

    #[test]
    fn nats_destination_is_subject_as_name_with_nothing_else_ever_filled() {
        let destination = nats_destination("orders.created");
        assert_eq!(destination.name.as_deref(), Some("orders.created"));
        assert_eq!(destination.namespace, None, "account is never client-visible");
        assert_eq!(destination.via, None, "a subject is not routed through anything separate");
        assert_eq!(destination.route, None);
        assert_eq!(nats_destination("  ").name, None, "whitespace is no subject");
    }

    #[test]
    fn nats_publish_label_reuses_the_generic_chain_against_the_nats_floor() {
        let destination = nats_destination("orders.created");
        assert_eq!(labeled("PUBLISH", &destination, SYSTEM_NATS), "PUBLISH orders.created");
        assert_eq!(
            labeled("PUBLISH", &nats_destination(""), SYSTEM_NATS),
            "PUBLISH nats"
        );
    }

    #[test]
    fn jetstream_poll_destination_names_no_place_only_stream_and_consumer() {
        let destination = nats_jetstream_poll_destination("ORDERS", "worker-1");
        assert_eq!(destination.name, None, "a batch can span subjects — no single place");
        assert_eq!(destination.via.as_deref(), Some("ORDERS"));
        assert_eq!(destination.route.as_deref(), Some("worker-1"));
        // Blank facts are absence, same rule as everywhere else.
        let blank = nats_jetstream_poll_destination("", "");
        assert_eq!(blank.via, None);
        assert_eq!(blank.route, None);
    }

    #[test]
    fn jetstream_next_subject_splits_the_fixed_prefix_exactly() {
        assert_eq!(
            jetstream_next_subject_stream_and_consumer("$JS.API.CONSUMER.MSG.NEXT.ORDERS.worker-1"),
            Some(("ORDERS".to_owned(), "worker-1".to_owned()))
        );
    }

    #[test]
    fn jetstream_next_subject_is_absent_for_anything_that_does_not_match_exactly() {
        // Wrong / no prefix at all.
        assert_eq!(jetstream_next_subject_stream_and_consumer("ORDERS.worker-1"), None);
        // Missing the consumer token.
        assert_eq!(
            jetstream_next_subject_stream_and_consumer("$JS.API.CONSUMER.MSG.NEXT.ORDERS"),
            None
        );
        // A third token where subject grammar guarantees there cannot be one —
        // absent rather than silently taking the first two.
        assert_eq!(
            jetstream_next_subject_stream_and_consumer("$JS.API.CONSUMER.MSG.NEXT.ORDERS.worker-1.extra"),
            None
        );
    }
}
