//! On-device discovery of text that may need redaction.
//!
//! Findings are intentionally raw-free. Recognized text exists only for the
//! duration of one scan; returned values contain category, source geometry,
//! confidence, a non-secret reason, and the immutable content revision. That
//! makes the result safe to cache in memory and safe to format in diagnostics
//! without reproducing the secret it points at.

use std::{
    collections::VecDeque,
    net::{Ipv4Addr, Ipv6Addr},
    ops::Range,
    str::FromStr as _,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use regex::Regex;
use scrozz_core::{ContentRevision, Error, Frame, LogicalRect, Result};

use crate::{Ocr, TextBlock};

const HIGH_CONFIDENCE: u16 = 800;
const MAX_INTERNAL_CANDIDATES: usize = 8192;

/// Identity of one finding within a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingId(u64);

impl FindingId {
    /// Stable ordinal within the containing scan.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Confidence in thousandths, from 0 to 1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingConfidence(u16);

impl FindingConfidence {
    /// Creates a clamped confidence value.
    #[must_use]
    pub const fn from_milli(value: u16) -> Self {
        Self(if value > 1000 { 1000 } else { value })
    }

    /// Confidence as a value in `0.0..=1.0`.
    #[must_use]
    pub fn as_f32(self) -> f32 {
        f32::from(self.0) / 1000.0
    }

    /// Whether this meets the default high-confidence review threshold.
    #[must_use]
    pub const fn is_high(self) -> bool {
        self.0 >= HIGH_CONFIDENCE
    }
}

/// A clear, user-facing category of possible sensitive information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SensitiveCategory {
    /// An email address.
    EmailAddress,
    /// A Luhn-valid payment-card candidate.
    PaymentCard,
    /// An IPv4 network address.
    Ipv4Address,
    /// An IPv6 network address.
    Ipv6Address,
    /// A telephone-number candidate.
    PhoneNumber,
    /// A URL query or fragment carrying an access value.
    TokenizedUrl,
    /// A likely API key, access token, password, or secret.
    SecretKey,
}

impl SensitiveCategory {
    /// Plain-language accessible label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::EmailAddress => "Email address",
            Self::PaymentCard => "Payment card number",
            Self::Ipv4Address => "IPv4 address",
            Self::Ipv6Address => "IPv6 address",
            Self::PhoneNumber => "Phone number",
            Self::TokenizedUrl => "URL access value",
            Self::SecretKey => "Secret or API key",
        }
    }
}

/// Why a candidate received its confidence, without including its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FindingReason {
    /// A syntactically well-formed address.
    StructuredEmail,
    /// Luhn validation plus nearby payment terminology.
    LuhnWithPaymentContext,
    /// Luhn validation without corroborating context.
    LuhnCandidate,
    /// A valid network address with nearby host/network terminology.
    NetworkAddressWithContext,
    /// A valid network address without corroborating context.
    NetworkAddressCandidate,
    /// A plausible phone number with nearby contact terminology.
    PhoneWithContext,
    /// A plausible phone number without corroborating context.
    PhoneCandidate,
    /// A sensitive query/fragment parameter in a URL.
    SensitiveUrlParameter,
    /// A provider-specific key prefix with the expected shape.
    RecognizedSecretPrefix,
    /// An explicit HTTP Authorization credential.
    AuthorizationCredential,
    /// A high-entropy value beside explicit secret terminology.
    EntropyWithSecretContext,
}

impl FindingReason {
    /// Short review explanation that never reproduces source text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StructuredEmail => "Well-formed email address",
            Self::LuhnWithPaymentContext => "Valid checksum near payment wording",
            Self::LuhnCandidate => "Valid payment-card checksum",
            Self::NetworkAddressWithContext => "Valid address near network wording",
            Self::NetworkAddressCandidate => "Valid network address",
            Self::PhoneWithContext => "Phone-shaped number near contact wording",
            Self::PhoneCandidate => "Phone-shaped number",
            Self::SensitiveUrlParameter => "Access value in a URL parameter",
            Self::RecognizedSecretPrefix => "Recognized credential prefix",
            Self::AuthorizationCredential => "Explicit authorization credential",
            Self::EntropyWithSecretContext => "Random-looking value near secret wording",
        }
    }
}

/// The kind of content that produced a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SensitiveSource {
    /// A screenshot or still image.
    Image,
    /// One independently revisioned video frame.
    VideoFrame {
        /// Frame ordinal supplied by the caller.
        index: u64,
    },
    /// A decoded barcode payload.
    Barcode,
}

/// A raw-free pointer to possible sensitive information.
#[derive(Debug, Clone, PartialEq)]
pub struct SensitiveFinding {
    id: FindingId,
    category: SensitiveCategory,
    bounds: LogicalRect,
    confidence: FindingConfidence,
    reason: FindingReason,
    block_index: usize,
    byte_range: Range<usize>,
    revision: ContentRevision,
}

impl SensitiveFinding {
    /// Finding identity within this scan.
    #[must_use]
    pub const fn id(&self) -> FindingId {
        self.id
    }

    /// Detected category.
    #[must_use]
    pub const fn category(&self) -> SensitiveCategory {
        self.category
    }

    /// Source-coordinate bounds suitable for a review overlay or redaction.
    #[must_use]
    pub const fn bounds(&self) -> LogicalRect {
        self.bounds
    }

    /// Detector confidence.
    #[must_use]
    pub const fn confidence(&self) -> FindingConfidence {
        self.confidence
    }

    /// Non-secret confidence explanation.
    #[must_use]
    pub const fn reason(&self) -> FindingReason {
        self.reason
    }

    /// OCR block that contained the finding.
    #[must_use]
    pub const fn block_index(&self) -> usize {
        self.block_index
    }

    /// Byte range within the ephemeral OCR block.
    ///
    /// The range is positional metadata only. The scan does not retain the text.
    #[must_use]
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_range.clone()
    }

    /// Content revision this finding describes.
    #[must_use]
    pub const fn revision(&self) -> ContentRevision {
        self.revision
    }
}

/// Completed raw-free scan results.
#[derive(Debug, Clone, PartialEq)]
pub struct SensitiveScan {
    revision: ContentRevision,
    source: SensitiveSource,
    findings: Vec<SensitiveFinding>,
    truncated: bool,
}

impl SensitiveScan {
    /// Content revision this scan describes.
    #[must_use]
    pub const fn revision(&self) -> ContentRevision {
        self.revision
    }

    /// Source kind.
    #[must_use]
    pub const fn source(&self) -> SensitiveSource {
        self.source
    }

    /// Findings in deterministic visual order.
    #[must_use]
    pub fn findings(&self) -> &[SensitiveFinding] {
        &self.findings
    }

    /// Whether the fixed result limit omitted additional candidates.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// Fixed resource and review thresholds for one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveScanOptions {
    /// Whether review-confidence candidates are returned.
    pub include_review_confidence: bool,
    /// Minimum confidence returned when review-confidence candidates are shown.
    pub minimum_confidence: FindingConfidence,
    /// Maximum OCR blocks accepted.
    pub max_blocks: usize,
    /// Maximum total recognized UTF-8 bytes accepted.
    pub max_text_bytes: usize,
    /// Maximum findings returned.
    pub max_findings: usize,
    /// Maximum detector time after OCR completes.
    pub analysis_timeout: Duration,
}

impl Default for SensitiveScanOptions {
    fn default() -> Self {
        Self {
            include_review_confidence: true,
            minimum_confidence: FindingConfidence::from_milli(600),
            max_blocks: 4096,
            max_text_bytes: 1024 * 1024,
            max_findings: 512,
            analysis_timeout: Duration::from_secs(2),
        }
    }
}

/// Cooperative cancellation shared with an analysis worker.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// A live token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Deterministic local sensitive-information detector.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalSensitiveDetector;

impl LocalSensitiveDetector {
    /// Creates a detector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Scans OCR blocks without retaining their text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Cancelled`] when cancelled, or
    /// [`Error::InvalidRequest`] when resource limits are invalid/exceeded.
    pub fn scan_blocks(
        &self,
        revision: ContentRevision,
        source: SensitiveSource,
        blocks: &[TextBlock],
        options: &SensitiveScanOptions,
        cancellation: &CancellationToken,
    ) -> Result<SensitiveScan> {
        validate_options(options)?;
        if blocks.len() > options.max_blocks {
            return Err(Error::InvalidRequest(format!(
                "sensitive-information scan received {} OCR blocks; limit is {}",
                blocks.len(),
                options.max_blocks
            )));
        }
        let bytes = blocks.iter().try_fold(0usize, |total, block| {
            total.checked_add(block.text.len()).ok_or_else(|| {
                Error::InvalidRequest("sensitive-information text size overflowed".to_owned())
            })
        })?;
        if bytes > options.max_text_bytes {
            return Err(Error::InvalidRequest(format!(
                "sensitive-information scan received {bytes} text bytes; limit is {}",
                options.max_text_bytes
            )));
        }

        let deadline = Instant::now()
            .checked_add(options.analysis_timeout)
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "sensitive-information analysis timeout is too large".to_owned(),
                )
            })?;
        check_progress(cancellation, deadline)?;
        let mut candidates = Vec::new();
        let mut candidate_truncated = false;
        let candidate_limit = options
            .max_findings
            .saturating_mul(8)
            .clamp(1, MAX_INTERNAL_CANDIDATES);
        for (block_index, block) in blocks.iter().enumerate() {
            check_progress(cancellation, deadline)?;
            let start = candidates.len();
            detect_block(
                &block.text,
                &mut CandidateCollector {
                    items: &mut candidates,
                    limit: candidate_limit,
                    truncated: &mut candidate_truncated,
                    cancellation,
                    deadline,
                },
            )?;
            check_progress(cancellation, deadline)?;
            for candidate in &mut candidates[start..] {
                candidate.block_index = Some(block_index);
                candidate.confidence = adjust_for_ocr(candidate.confidence, block.confidence);
            }
            if candidate_truncated {
                break;
            }
        }

        candidates.sort_by(|left, right| {
            left.block_index
                .cmp(&right.block_index)
                .then_with(|| left.span.start.cmp(&right.span.start))
                .then_with(|| right.confidence.cmp(&left.confidence))
                .then_with(|| left.category.cmp(&right.category))
        });
        let mut deduplicated = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if deduplicated.last().is_some_and(|previous: &Candidate| {
                previous.block_index == candidate.block_index && previous.span == candidate.span
            }) {
                continue;
            }
            deduplicated.push(candidate);
        }

        let mut findings = Vec::with_capacity(deduplicated.len().min(options.max_findings));
        let mut truncated = candidate_truncated;
        for candidate in deduplicated {
            check_progress(cancellation, deadline)?;
            let confidence = FindingConfidence::from_milli(candidate.confidence);
            if confidence < options.minimum_confidence
                || (!options.include_review_confidence && !confidence.is_high())
            {
                continue;
            }
            if findings.len() >= options.max_findings {
                truncated = true;
                break;
            }
            let block_index = candidate
                .block_index
                .expect("candidates are assigned while scanning their block");
            let block = &blocks[block_index];
            findings.push(SensitiveFinding {
                id: FindingId(findings.len() as u64 + 1),
                category: candidate.category,
                // OCR backends expose line/block geometry, not glyph geometry.
                // A proportional substring box can leave variable-width glyphs
                // visible, so privacy suggestions conservatively cover the
                // complete OCR block. Barcode scans likewise cover the whole
                // symbol, because every module contributes to the payload.
                bounds: block.bounds,
                confidence,
                reason: candidate.reason,
                block_index,
                byte_range: candidate.span,
                revision,
            });
        }
        check_progress(cancellation, deadline)?;

        Ok(SensitiveScan {
            revision,
            source,
            findings,
            truncated,
        })
    }

    /// Runs OCR and scans one screenshot, image, or video frame.
    ///
    /// OCR backends already impose their own platform deadline. Cancellation is
    /// observed before and after that bounded call and throughout local analysis.
    ///
    /// # Errors
    ///
    /// Propagates malformed-frame, OCR, cancellation, and resource-limit errors.
    pub fn scan_frame(
        &self,
        ocr: &dyn Ocr,
        frame: &Frame,
        revision: ContentRevision,
        source: SensitiveSource,
        options: &SensitiveScanOptions,
        cancellation: &CancellationToken,
    ) -> Result<SensitiveScan> {
        validate_options(options)?;
        if matches!(source, SensitiveSource::Barcode) {
            return Err(Error::InvalidRequest(
                "a barcode source must be scanned from its decoded payload".to_owned(),
            ));
        }
        if !frame.is_well_formed() {
            return Err(Error::InvalidRequest(
                "sensitive-information scan received a malformed image frame".to_owned(),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let blocks = ocr.recognize(frame)?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.scan_blocks(revision, source, &blocks, options, cancellation)
    }

    /// Scans a decoded barcode payload locally.
    ///
    /// # Errors
    ///
    /// As [`Self::scan_blocks`].
    pub fn scan_barcode(
        &self,
        payload: &str,
        bounds: LogicalRect,
        revision: ContentRevision,
        options: &SensitiveScanOptions,
        cancellation: &CancellationToken,
    ) -> Result<SensitiveScan> {
        let block = TextBlock {
            text: payload.to_owned(),
            bounds,
            confidence: 1.0,
        };
        self.scan_blocks(
            revision,
            SensitiveSource::Barcode,
            &[block],
            options,
            cancellation,
        )
    }
}

/// Small in-memory cache keyed only by immutable revision.
#[derive(Debug)]
pub struct SensitiveScanCache {
    capacity: usize,
    scans: VecDeque<Arc<SensitiveScan>>,
}

impl SensitiveScanCache {
    /// Creates a cache with a fixed positive capacity.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for zero capacity.
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::InvalidRequest(
                "sensitive-information cache capacity must be positive".to_owned(),
            ));
        }
        Ok(Self {
            capacity,
            scans: VecDeque::with_capacity(capacity),
        })
    }

    /// Retrieves a scan for exactly `revision`.
    #[must_use]
    pub fn get(&self, revision: ContentRevision) -> Option<Arc<SensitiveScan>> {
        self.scans
            .iter()
            .find(|scan| scan.revision == revision)
            .map(Arc::clone)
    }

    /// Inserts a raw-free scan, replacing the same revision and evicting oldest.
    pub fn insert(&mut self, scan: SensitiveScan) -> Arc<SensitiveScan> {
        self.scans
            .retain(|existing| existing.revision != scan.revision);
        let scan = Arc::new(scan);
        self.scans.push_back(Arc::clone(&scan));
        while self.scans.len() > self.capacity {
            self.scans.pop_front();
        }
        scan
    }

    /// Removes every cached revision.
    pub fn clear(&mut self) {
        self.scans.clear();
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    category: SensitiveCategory,
    span: Range<usize>,
    confidence: u16,
    reason: FindingReason,
    block_index: Option<usize>,
}

struct CandidateCollector<'a> {
    items: &'a mut Vec<Candidate>,
    limit: usize,
    truncated: &'a mut bool,
    cancellation: &'a CancellationToken,
    deadline: Instant,
}

impl CandidateCollector<'_> {
    fn push(&mut self, candidate: Candidate) -> Result<bool> {
        check_progress(self.cancellation, self.deadline)?;
        if self.items.len() >= self.limit {
            *self.truncated = true;
            return Ok(false);
        }
        self.items.push(candidate);
        Ok(true)
    }

    fn is_truncated(&self) -> bool {
        *self.truncated
    }
}

fn validate_options(options: &SensitiveScanOptions) -> Result<()> {
    if options.max_blocks == 0
        || options.max_text_bytes == 0
        || options.max_findings == 0
        || options.analysis_timeout.is_zero()
    {
        return Err(Error::InvalidRequest(
            "sensitive-information scan limits must be positive".to_owned(),
        ));
    }
    Ok(())
}

fn check_progress(cancellation: &CancellationToken, deadline: Instant) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Error::InvalidRequest(
            "sensitive-information analysis exceeded its time limit".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn detect_block(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    detect_emails(text, out)?;
    if out.is_truncated() {
        return Ok(());
    }
    let card_start = out.items.len();
    detect_cards(text, out)?;
    if out.is_truncated() {
        return Ok(());
    }
    let payment_cards: Vec<Range<usize>> = out.items[card_start..]
        .iter()
        .filter(|candidate| candidate.reason == FindingReason::LuhnWithPaymentContext)
        .map(|candidate| candidate.span.clone())
        .collect();
    detect_network_addresses(text, out)?;
    if out.is_truncated() {
        return Ok(());
    }
    detect_phones(text, &payment_cards, out)?;
    if out.is_truncated() {
        return Ok(());
    }
    detect_tokenized_urls(text, out)?;
    if out.is_truncated() {
        return Ok(());
    }
    detect_secrets(text, out)
}

fn detect_emails(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    for found in email_regex().find_iter(text) {
        if !out.push(Candidate {
            category: SensitiveCategory::EmailAddress,
            span: found.range(),
            confidence: 920,
            reason: FindingReason::StructuredEmail,
            block_index: None,
        })? {
            break;
        }
    }
    Ok(())
}

fn detect_cards(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    for found in card_regex().find_iter(text) {
        if adjacent_decimal_digit(text, &found.range()) {
            continue;
        }
        let digits = ascii_digits(found.as_str());
        if !(13..=19).contains(&digits.len()) || repeated_digit(&digits) || !luhn_valid(&digits) {
            continue;
        }
        let payment_context = has_context(
            text,
            &found.range(),
            &[
                "card",
                "credit",
                "debit",
                "payment",
                "visa",
                "mastercard",
                "amex",
                "pan",
            ],
        );
        if !out.push(Candidate {
            category: SensitiveCategory::PaymentCard,
            span: trim_span(text, found.range()),
            confidence: if payment_context { 970 } else { 690 },
            reason: if payment_context {
                FindingReason::LuhnWithPaymentContext
            } else {
                FindingReason::LuhnCandidate
            },
            block_index: None,
        })? {
            break;
        }
    }
    Ok(())
}

fn detect_network_addresses(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    for found in ipv4_regex().find_iter(text) {
        if Ipv4Addr::from_str(found.as_str()).is_err() {
            continue;
        }
        let context = has_context(
            text,
            &found.range(),
            &["ip", "address", "host", "server", "vpn", "ssh", "gateway"],
        );
        if !out.push(Candidate {
            category: SensitiveCategory::Ipv4Address,
            span: found.range(),
            confidence: if context { 860 } else { 620 },
            reason: if context {
                FindingReason::NetworkAddressWithContext
            } else {
                FindingReason::NetworkAddressCandidate
            },
            block_index: None,
        })? {
            return Ok(());
        }
    }

    for captures in bracketed_ipv6_regex().captures_iter(text) {
        let Some(address) = captures.name("address") else {
            continue;
        };
        let candidate = address
            .as_str()
            .split_once('%')
            .map_or(address.as_str(), |(address, _zone)| address);
        if Ipv6Addr::from_str(candidate).is_err() {
            continue;
        }
        let context = has_context(
            text,
            &address.range(),
            &["ip", "address", "host", "server", "vpn", "ssh", "gateway"],
        );
        if !out.push(Candidate {
            category: SensitiveCategory::Ipv6Address,
            span: address.range(),
            confidence: if context { 870 } else { 650 },
            reason: if context {
                FindingReason::NetworkAddressWithContext
            } else {
                FindingReason::NetworkAddressCandidate
            },
            block_index: None,
        })? {
            return Ok(());
        }
    }

    for found in ipv6_token_regex().find_iter(text) {
        let span = trim_span(text, found.range());
        if span.is_empty() {
            continue;
        }
        let mut token = text[span.clone()].trim_matches(['[', ']']);
        if let Some((address, _zone)) = token.split_once('%') {
            token = address;
        }
        if !token.contains(':') || Ipv6Addr::from_str(token).is_err() {
            continue;
        }
        let context = has_context(
            text,
            &span,
            &["ip", "address", "host", "server", "vpn", "ssh", "gateway"],
        );
        if !out.push(Candidate {
            category: SensitiveCategory::Ipv6Address,
            span,
            confidence: if context { 870 } else { 650 },
            reason: if context {
                FindingReason::NetworkAddressWithContext
            } else {
                FindingReason::NetworkAddressCandidate
            },
            block_index: None,
        })? {
            return Ok(());
        }
    }
    Ok(())
}

fn detect_phones(
    text: &str,
    payment_cards: &[Range<usize>],
    out: &mut CandidateCollector<'_>,
) -> Result<()> {
    for found in phone_regex().find_iter(text) {
        for span in split_phone_span(text, found.range()) {
            if !detect_phone_span(text, span, payment_cards, out)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn detect_phone_span(
    text: &str,
    span: Range<usize>,
    payment_cards: &[Range<usize>],
    out: &mut CandidateCollector<'_>,
) -> Result<bool> {
    let digits = normalized_digits(&text[span.clone()]);
    if !(7..=15).contains(&digits.len()) {
        return Ok(true);
    }
    if overlaps_sorted(payment_cards, &span) {
        return Ok(true);
    }
    let context = text[span.clone()]
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("tel:")
        || has_context(
            text,
            &span,
            &[
                "phone",
                "mobile",
                "telephone",
                "call",
                "sms",
                "contact",
                "whatsapp",
            ],
        );
    if !context && (13..=15).contains(&digits.len()) && luhn_valid(&digits) {
        return Ok(true);
    }
    out.push(Candidate {
        category: SensitiveCategory::PhoneNumber,
        span: trim_span(text, span),
        confidence: if context { 860 } else { 610 },
        reason: if context {
            FindingReason::PhoneWithContext
        } else {
            FindingReason::PhoneCandidate
        },
        block_index: None,
    })
}

fn split_phone_span(text: &str, span: Range<usize>) -> Vec<Range<usize>> {
    let span = trim_span(text, span);
    let total_digits = normalized_digits(&text[span.clone()]).len();
    if total_digits <= 15 {
        return vec![span];
    }
    let mut spans = Vec::new();
    let mut start = span.start;
    let mut consumed_digits = 0usize;
    let mut current_digits = 0usize;
    for (offset, character) in text[span.clone()].char_indices() {
        if decimal_digit(character).is_some() {
            current_digits += 1;
            consumed_digits += 1;
        }
        if !character.is_whitespace()
            || !(7..=15).contains(&current_digits)
            || total_digits.saturating_sub(consumed_digits) < 7
        {
            continue;
        }
        let split = span.start + offset;
        spans.push(trim_span(text, start..split));
        start = split + character.len_utf8();
        current_digits = 0;
    }
    let remainder = trim_span(text, start..span.end);
    if (7..=15).contains(&normalized_digits(&text[remainder.clone()]).len()) {
        spans.push(remainder);
    }
    spans
}

fn overlaps_sorted(ranges: &[Range<usize>], target: &Range<usize>) -> bool {
    let index = ranges.partition_point(|range| range.end <= target.start);
    ranges
        .get(index)
        .is_some_and(|range| range.start < target.end)
}

fn detect_tokenized_urls(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    for found in url_regex().find_iter(text) {
        let url = found.as_str();
        let fragment = url.find('#');
        if let Some(query) = url.find('?') {
            let end = fragment.unwrap_or(url.len());
            if query < end {
                detect_url_parameter_segment(&url[query + 1..end], found.start() + query + 1, out)?;
            }
        }
        if let Some(fragment) = fragment {
            detect_url_parameter_segment(&url[fragment + 1..], found.start() + fragment + 1, out)?;
        }
    }
    Ok(())
}

fn detect_url_parameter_segment(
    segment: &str,
    segment_start: usize,
    out: &mut CandidateCollector<'_>,
) -> Result<()> {
    let mut offset = 0;
    for parameter in segment.split(['&', ';']) {
        let parameter_start = offset;
        offset += parameter.len() + 1;
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if !sensitive_parameter(name) || placeholder(value) || value.len() < 6 {
            continue;
        }
        let value_start = segment_start + parameter_start + name.len() + 1;
        if !out.push(Candidate {
            category: SensitiveCategory::TokenizedUrl,
            span: value_start..value_start + value.len(),
            confidence: 980,
            reason: FindingReason::SensitiveUrlParameter,
            block_index: None,
        })? {
            break;
        }
    }
    Ok(())
}

fn detect_secrets(text: &str, out: &mut CandidateCollector<'_>) -> Result<()> {
    for found in known_secret_regex().find_iter(text) {
        if !placeholder(found.as_str())
            && !out.push(Candidate {
                category: SensitiveCategory::SecretKey,
                span: found.range(),
                confidence: 990,
                reason: FindingReason::RecognizedSecretPrefix,
                block_index: None,
            })?
        {
            return Ok(());
        }
    }

    for captures in assigned_secret_regex().captures_iter(text) {
        let Some(value) = captures.name("value") else {
            continue;
        };
        let value_text = value.as_str();
        if placeholder(value_text) {
            continue;
        }
        if captures.name("scheme").is_some() && value_text.len() >= 8 {
            if !out.push(Candidate {
                category: SensitiveCategory::SecretKey,
                span: value.range(),
                confidence: 980,
                reason: FindingReason::AuthorizationCredential,
                block_index: None,
            })? {
                return Ok(());
            }
            continue;
        }
        if value_text.len() < 16 {
            continue;
        }
        let entropy = shannon_entropy(value_text.as_bytes());
        let classes = character_classes(value_text);
        let confidence = if value_text.len() >= 24 && entropy >= 3.5 && classes >= 3 {
            940
        } else if entropy >= 3.2 && classes >= 2 {
            690
        } else {
            continue;
        };
        if !out.push(Candidate {
            category: SensitiveCategory::SecretKey,
            span: value.range(),
            confidence,
            reason: FindingReason::EntropyWithSecretContext,
            block_index: None,
        })? {
            return Ok(());
        }
    }
    Ok(())
}

fn adjust_for_ocr(confidence: u16, ocr: f32) -> u16 {
    let ocr = if ocr.is_finite() {
        ocr.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let factor = 0.75 + f64::from(ocr) * 0.25;
    (f64::from(confidence) * factor).round().clamp(0.0, 1000.0) as u16
}

fn has_context(text: &str, span: &Range<usize>, terms: &[&str]) -> bool {
    let start = floor_char_boundary(text, span.start.saturating_sub(48));
    let end = ceil_char_boundary(text, (span.end + 48).min(text.len()));
    let context = text[start..end].to_lowercase();
    context
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| terms.contains(&word))
}

fn adjacent_decimal_digit(text: &str, span: &Range<usize>) -> bool {
    text[..span.start]
        .chars()
        .next_back()
        .and_then(decimal_digit)
        .is_some()
        || text[span.end..]
            .chars()
            .next()
            .and_then(decimal_digit)
            .is_some()
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn trim_span(text: &str, mut span: Range<usize>) -> Range<usize> {
    while span.start < span.end {
        let Some(character) = text[span.start..span.end].chars().next() else {
            break;
        };
        if !matches!(character, ' ' | '\t' | '(' | '[' | '{' | '"' | '\'') {
            break;
        }
        span.start += character.len_utf8();
    }
    while span.start < span.end {
        let Some(character) = text[span.start..span.end].chars().next_back() else {
            break;
        };
        if !matches!(
            character,
            ' ' | '\t' | ')' | ']' | '}' | '"' | '\'' | ',' | ';' | '.'
        ) {
            break;
        }
        span.end -= character.len_utf8();
    }
    span
}

fn ascii_digits(text: &str) -> Vec<u8> {
    text.bytes()
        .filter(u8::is_ascii_digit)
        .map(|byte| byte - b'0')
        .collect()
}

fn normalized_digits(text: &str) -> Vec<u8> {
    text.chars().filter_map(decimal_digit).collect()
}

fn decimal_digit(character: char) -> Option<u8> {
    let code = character as u32;
    const ZEROES: [u32; 18] = [
        0x0030, 0x0660, 0x06f0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6,
        0x0d66, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x17e0, 0xff10,
    ];
    ZEROES.iter().find_map(|zero| {
        let value = code.checked_sub(*zero)?;
        (value <= 9).then_some(value as u8)
    })
}

fn luhn_valid(digits: &[u8]) -> bool {
    if digits.is_empty() {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            let mut value = u32::from(*digit);
            if index % 2 == 1 {
                value *= 2;
                if value > 9 {
                    value -= 9;
                }
            }
            value
        })
        .sum();
    sum.is_multiple_of(10)
}

fn repeated_digit(digits: &[u8]) -> bool {
    digits
        .first()
        .is_some_and(|first| digits.iter().all(|digit| digit == first))
}

fn sensitive_parameter(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "token"
            | "access_token"
            | "api_key"
            | "apikey"
            | "secret"
            | "signature"
            | "sig"
            | "auth"
            | "password"
            | "session"
            | "session_id"
            | "code"
    )
}

fn placeholder(value: &str) -> bool {
    let normalized = value
        .trim_matches(['"', '\'', '<', '>', '{', '}'])
        .to_ascii_lowercase();
    normalized.is_empty()
        || normalized.contains("example")
        || normalized.contains("changeme")
        || normalized.contains("replace_me")
        || normalized.contains("your_")
        || normalized.contains("placeholder")
        || normalized
            .bytes()
            .all(|byte| byte == b'x' || byte == b'*' || byte == b'0')
}

fn character_classes(value: &str) -> usize {
    [
        value.bytes().any(|byte| byte.is_ascii_lowercase()),
        value.bytes().any(|byte| byte.is_ascii_uppercase()),
        value.bytes().any(|byte| byte.is_ascii_digit()),
        value
            .bytes()
            .any(|byte| matches!(byte, b'_' | b'-' | b'/' | b'+' | b'=' | b'.')),
    ]
    .into_iter()
    .filter(|present| *present)
    .count()
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in bytes {
        counts[usize::from(*byte)] += 1;
    }
    let length = bytes.len() as f64;
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let probability = count as f64 / length;
            -probability * probability.log2()
        })
        .sum()
}

fn email_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?iu)[\p{L}\p{N}][\p{L}\p{N}._%+\-]{0,63}@(?:[\p{L}\p{N}](?:[\p{L}\p{N}\-]{0,61}[\p{L}\p{N}])?\.)+\p{L}{2,63}",
        )
        .expect("email detector pattern is valid")
    })
}

fn card_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?:[0-9][ -]?){12,18}[0-9]").expect("card detector pattern is valid")
    })
}

fn ipv4_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").expect("IPv4 detector pattern is valid")
    })
}

fn ipv6_token_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)[0-9a-f:\[\]%.]{3,}").expect("IPv6 token pattern is valid")
    })
}

fn bracketed_ipv6_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)\[(?P<address>[0-9a-f:%\.]+)\](?::[0-9]{1,5})?")
            .expect("bracketed IPv6 pattern is valid")
    })
}

fn phone_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?iu)(?:tel:\s*)?\+?\p{Nd}[\p{Nd}() .-]{5,}\p{Nd}")
            .expect("phone detector pattern is valid")
    })
}

fn url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)\bhttps?://[^\s<>"']+"#).expect("URL detector pattern is valid")
    })
}

fn known_secret_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?x)
            \b(?:AKIA|ASIA)[A-Z0-9]{16}\b |
            \bgh[pousr]_[A-Za-z0-9]{20,255}\b |
            \bgithub_pat_[A-Za-z0-9_]{20,255}\b |
            \bsk-[A-Za-z0-9_-]{20,}\b |
            \bxox[baprs]-[A-Za-z0-9-]{20,}\b |
            \bAIza[A-Za-z0-9_-]{30,}\b
            ",
        )
        .expect("known-secret detector pattern is valid")
    })
}

fn assigned_secret_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            ["']?
            (?:api(?:[_ -]?key)|secret|token|password|passwd|client(?:[_ -]?secret)|authorization)
            ["']?\s*[:=]\s*["']?
            (?:(?P<scheme>bearer|basic)\s+)?
            (?P<value>[A-Za-z0-9_./+=-]{8,})
            "#,
        )
        .expect("assigned-secret detector pattern is valid")
    })
}

#[cfg(test)]
mod tests {
    use scrozz_core::{
        ColorSpace, LogicalPoint, LogicalSize, PhysicalSize, PixelFormat, ScaleFactor,
    };

    use super::*;

    fn block(text: &str) -> TextBlock {
        TextBlock {
            text: text.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(10.0, 20.0), LogicalSize::new(600.0, 24.0)),
            confidence: 0.98,
        }
    }

    fn scan(text: &str, include_review: bool) -> SensitiveScan {
        LocalSensitiveDetector::new()
            .scan_blocks(
                ContentRevision::new(7),
                SensitiveSource::Image,
                &[block(text)],
                &SensitiveScanOptions {
                    include_review_confidence: include_review,
                    ..SensitiveScanOptions::default()
                },
                &CancellationToken::new(),
            )
            .expect("scan")
    }

    #[test]
    fn detects_high_confidence_categories_without_retaining_values() {
        let source = concat!(
            "email ada@example.com card 4111 1111 1111 1111 ",
            "server ip 10.20.30.40 phone +1 (212) 555-0198 ",
            "https://example.invalid/cb?access_token=a9B8c7D6e5F4g3H2 ",
            "api_key=aB9xK2mQ7pL5nR8wT3vY6zC1dF4gH"
        );
        let result = scan(source, false);
        let categories: Vec<_> = result
            .findings()
            .iter()
            .map(SensitiveFinding::category)
            .collect();

        for expected in [
            SensitiveCategory::EmailAddress,
            SensitiveCategory::PaymentCard,
            SensitiveCategory::Ipv4Address,
            SensitiveCategory::PhoneNumber,
            SensitiveCategory::TokenizedUrl,
            SensitiveCategory::SecretKey,
        ] {
            assert!(categories.contains(&expected), "missing {expected:?}");
        }
        let debug = format!("{result:?}");
        for secret in [
            "ada@example.com",
            "4111",
            "10.20.30.40",
            "555-0198",
            "a9B8c7",
            "aB9xK2",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}");
        }
    }

    #[test]
    fn supports_international_email_and_phone_digits() {
        let result = scan(
            "Contact δοκιμή@παράδειγμα.δοκιμή, phone +٩٧١ ٥٠ ١٢٣ ٤٥٦٧",
            false,
        );
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::EmailAddress)
        );
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::PhoneNumber)
        );
    }

    #[test]
    fn luhn_without_payment_context_is_review_only() {
        assert!(scan("4111111111111111", false).findings().is_empty());
        assert!(
            scan("4111111111111111", true)
                .findings()
                .iter()
                .any(|finding| finding.reason() == FindingReason::LuhnCandidate)
        );
    }

    #[test]
    fn formatted_payment_card_is_not_duplicated_as_phone_numbers() {
        let result = scan("payment card 4111 1111 1111 1111", true);
        assert_eq!(
            result
                .findings()
                .iter()
                .filter(|finding| finding.category() == SensitiveCategory::PaymentCard)
                .count(),
            1
        );
        assert!(
            !result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::PhoneNumber)
        );
    }

    #[test]
    fn rejects_card_and_secret_boundary_false_positives() {
        for text in [
            "card 0000 0000 0000 0000",
            "card 4532 0151 3467 5854",
            "api_key=abc123",
            "token=xxxxxxxxxxxxxxxxxxxxxxxx",
            "release v1.2.3.4",
        ] {
            assert!(scan(text, false).findings().is_empty(), "{text}");
        }
    }

    #[test]
    fn review_confidence_is_hidden_unless_requested() {
        assert!(scan("connect to 192.0.2.42", false).findings().is_empty());
        let review = scan("connect to 192.0.2.42", true);
        assert_eq!(review.findings().len(), 1);
        assert!(!review.findings()[0].confidence().is_high());
    }

    #[test]
    fn detects_ipv6_known_keys_and_token_urls_without_echoing_them() {
        let source = concat!(
            "server address 2001:db8::8a2e:370:7334 ",
            "AKIAQWERTYUIOP123456 ",
            "https://example.invalid/callback?token=Ab9Cd8Ef7Gh6"
        );
        let result = scan(source, false);
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::Ipv6Address)
        );
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.reason() == FindingReason::RecognizedSecretPrefix)
        );
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::TokenizedUrl)
        );
        let debug = format!("{result:?}");
        assert!(!debug.contains("AKIAQW"));
        assert!(!debug.contains("Ab9Cd8"));
    }

    #[test]
    fn scans_url_query_and_fragment_as_separate_parameter_sets() {
        let text = "https://host.invalid/cb?state=ordinary#access_token=Ab9Cd8Ef7Gh6";
        let result = scan(text, false);
        let finding = result
            .findings()
            .iter()
            .find(|finding| finding.category() == SensitiveCategory::TokenizedUrl)
            .expect("fragment token");
        assert_eq!(&text[finding.byte_range()], "Ab9Cd8Ef7Gh6");
    }

    #[test]
    fn detects_quoted_and_authorization_secret_assignments() {
        for source in [
            r#""api_key": "aB9xK2mQ7pL5nR8wT3vY6zC1dF4gH""#,
            "Authorization: Basic dXNlcjpwYXNz",
            "Authorization: Bearer aB9xK2mQ7pL5nR8wT3vY6zC1dF4gH",
            "client secret = aB9xK2mQ7pL5nR8wT3vY6zC1dF4gH",
        ] {
            assert!(
                scan(source, false)
                    .findings()
                    .iter()
                    .any(|finding| finding.category() == SensitiveCategory::SecretKey)
            );
        }
    }

    #[test]
    fn explicit_phone_context_wins_over_a_luhn_coincidence() {
        let result = scan("phone 378282246310005", false);
        assert!(
            result
                .findings()
                .iter()
                .any(|finding| finding.category() == SensitiveCategory::PhoneNumber)
        );
    }

    #[test]
    fn adjacent_phone_numbers_are_scanned_independently() {
        let result = scan("phone 212-555-0198 415-555-0123", false);
        assert_eq!(
            result
                .findings()
                .iter()
                .filter(|finding| finding.category() == SensitiveCategory::PhoneNumber)
                .count(),
            2
        );
    }

    #[test]
    fn bracketed_ipv6_endpoint_keeps_the_address_without_the_port() {
        let source = "server [2001:db8::1]:443";
        let result = scan(source, false);
        let finding = result
            .findings()
            .iter()
            .find(|finding| finding.category() == SensitiveCategory::Ipv6Address)
            .expect("IPv6 address");
        assert_eq!(&source[finding.byte_range()], "2001:db8::1");
    }

    #[test]
    fn placeholders_and_overlong_digit_runs_are_not_candidates() {
        for text in [
            "https://example.invalid?token=YOUR_TOKEN_HERE",
            "api_key=000000000000000000000000",
            "card 41111111111111111111",
        ] {
            assert!(scan(text, true).findings().is_empty(), "{text}");
        }
    }

    #[test]
    fn finding_bounds_conservatively_cover_the_complete_source_block() {
        let result = scan("prefix ada@example.com suffix", false);
        let finding = &result.findings()[0];
        assert_eq!(finding.bounds(), block("x").bounds);
        assert_eq!(finding.revision(), ContentRevision::new(7));
        assert!(finding.byte_range().start < finding.byte_range().end);
    }

    #[test]
    fn result_limit_is_explicitly_reported() {
        let detector = LocalSensitiveDetector::new();
        let result = detector
            .scan_blocks(
                ContentRevision::INITIAL,
                SensitiveSource::Image,
                &[block(
                    "a@example.com b@example.com c@example.com d@example.com",
                )],
                &SensitiveScanOptions {
                    max_findings: 2,
                    ..SensitiveScanOptions::default()
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(result.findings().len(), 2);
        assert!(result.is_truncated());
    }

    #[test]
    fn cancellation_and_limits_fail_closed() {
        let detector = LocalSensitiveDetector::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            detector.scan_blocks(
                ContentRevision::INITIAL,
                SensitiveSource::Image,
                &[block("email ada@example.com")],
                &SensitiveScanOptions::default(),
                &cancel
            ),
            Err(Error::Cancelled)
        ));

        let error = detector
            .scan_blocks(
                ContentRevision::INITIAL,
                SensitiveSource::Image,
                &[block(&"a".repeat(128))],
                &SensitiveScanOptions {
                    max_text_bytes: 64,
                    ..SensitiveScanOptions::default()
                },
                &CancellationToken::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn malformed_frames_are_rejected_before_ocr() {
        struct NeverOcr;
        impl Ocr for NeverOcr {
            fn recognize(&self, _frame: &Frame) -> Result<Vec<TextBlock>> {
                panic!("malformed frame reached OCR")
            }
        }
        let frame = Frame {
            data: vec![0; 4],
            size: PhysicalSize::new(100.0, 100.0),
            stride: 400,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        };
        assert!(matches!(
            LocalSensitiveDetector::new().scan_frame(
                &NeverOcr,
                &frame,
                ContentRevision::INITIAL,
                SensitiveSource::Image,
                &SensitiveScanOptions::default(),
                &CancellationToken::new()
            ),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn video_frames_and_barcodes_keep_source_identity() {
        let detector = LocalSensitiveDetector::new();
        let options = SensitiveScanOptions::default();
        let cancellation = CancellationToken::new();
        let video = detector
            .scan_blocks(
                ContentRevision::new(3),
                SensitiveSource::VideoFrame { index: 42 },
                &[block("email ada@example.com")],
                &options,
                &cancellation,
            )
            .unwrap();
        assert_eq!(video.source(), SensitiveSource::VideoFrame { index: 42 });

        let barcode = detector
            .scan_barcode(
                "https://example.invalid?token=Ab9Cd8Ef7Gh6",
                block("x").bounds,
                ContentRevision::new(4),
                &options,
                &cancellation,
            )
            .unwrap();
        assert_eq!(barcode.source(), SensitiveSource::Barcode);
        assert_eq!(
            barcode.findings()[0].category(),
            SensitiveCategory::TokenizedUrl
        );
    }

    #[test]
    fn cache_keys_only_exact_revisions_and_evicts_oldest() {
        let mut cache = SensitiveScanCache::new(2).unwrap();
        for revision in 1..=3 {
            cache.insert(scan("email ada@example.com", false).with_revision_for_test(revision));
        }
        assert!(cache.get(ContentRevision::new(1)).is_none());
        assert!(cache.get(ContentRevision::new(2)).is_some());
        assert!(cache.get(ContentRevision::new(3)).is_some());
    }

    #[test]
    fn detector_is_deterministic_over_generated_inputs() {
        let detector = LocalSensitiveDetector::new();
        let options = SensitiveScanOptions {
            include_review_confidence: true,
            ..SensitiveScanOptions::default()
        };
        for seed in 0u64..128 {
            let text =
                format!("seed={seed:03} email user{seed}@example.invalid token={seed:016x}AaZz");
            let one = detector
                .scan_blocks(
                    ContentRevision::new(seed),
                    SensitiveSource::Image,
                    &[block(&text)],
                    &options,
                    &CancellationToken::new(),
                )
                .unwrap();
            let two = detector
                .scan_blocks(
                    ContentRevision::new(seed),
                    SensitiveSource::Image,
                    &[block(&text)],
                    &options,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(one, two);
            assert!(
                one.findings()
                    .iter()
                    .all(|finding| (0.0..=1.0).contains(&finding.confidence().as_f32()))
            );
        }
    }

    impl SensitiveScan {
        fn with_revision_for_test(mut self, revision: u64) -> Self {
            self.revision = ContentRevision::new(revision);
            for finding in &mut self.findings {
                finding.revision = self.revision;
            }
            self
        }
    }
}
