// SPDX-License-Identifier: Apache-2.0

//! The JSON a report holds. Every string in it is an enum's name, a key built from enum names, a version number or
//! the install id. Nothing here can hold a path, a name, file contents or an error message.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The layout version of the properties a report sends. It changes when a property changes meaning.
pub const SCHEMA: u16 = 1;

/// What one install recorded on one UTC day. Its keys are always those of a [`Counter`] or a [`Measure`]: only a
/// [`Recorder`](crate::Recorder) fills it, and reading one from JSON keeps only the keys of the closed set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredReport")]
pub struct DailyReport {
    /// Days since 1970-01-01.
    pub day: u32,
    /// Sums, by metric and then by label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) counters: BTreeMap<String, BTreeMap<String, u64>>,
    /// Distributions, by metric and then by label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

/// A [`DailyReport`] as a file holds it, before its keys are checked.
#[derive(Deserialize)]
struct StoredReport {
    day: u32,
    #[serde(default)]
    counters: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

/// Rebuilds the report from the closed set of counters and measures, so a key that is not one of theirs, or a
/// histogram with another shift, is dropped.
impl From<StoredReport> for DailyReport {
    fn from(stored: StoredReport) -> Self {
        let mut report = DailyReport {
            day: stored.day,
            ..DailyReport::default()
        };
        for (metric, label) in Counter::all().map(Counter::key) {
            if let Some(&count) = stored
                .counters
                .get(metric)
                .and_then(|labels| labels.get(label))
            {
                (report.counters.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), count);
            }
        }
        for (metric, label, shift) in Measure::all().map(Measure::key) {
            if let Some(histogram) = (stored.histograms.get(metric))
                .and_then(|labels| labels.get(label))
                .filter(|histogram| histogram.shift == shift)
            {
                (report.histograms.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), histogram.clone());
            }
        }
        report
    }
}

impl DailyReport {
    pub fn merge(&mut self, other: DailyReport) {
        for (metric, labels) in other.counters {
            let into = self.counters.entry(metric).or_default();
            for (label, count) in labels {
                let sum = into.entry(label).or_default();
                *sum = sum.saturating_add(count);
            }
        }
        for (metric, labels) in other.histograms {
            let into = self.histograms.entry(metric).or_default();
            for (label, histogram) in labels {
                into.entry(label)
                    .or_insert_with(|| Histogram::new(histogram.shift))
                    .merge(&histogram);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counters.is_empty() && self.histograms.is_empty()
    }
}

/// Values in 16 power-of-two buckets. Bucket 0 holds values below `2^shift`; bucket `i` from 1 to 14 holds values
/// from `2^(shift + i - 1)` up to `2^(shift + i)`; bucket 15 holds everything above. Histograms with the same shift
/// add bucket by bucket, so they merge across days and installs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub shift: u8,
    pub buckets: [u64; 16],
    pub count: u64,
    pub sum: u64,
}

impl Histogram {
    pub fn new(shift: u8) -> Self {
        Histogram {
            shift,
            buckets: [0; 16],
            count: 0,
            sum: 0,
        }
    }

    pub fn record(&mut self, value: u64) {
        let scaled = value >> self.shift.min(63);
        let bucket = (64 - scaled.leading_zeros()).min(15) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
        self.sum = self.sum.saturating_add(value);
    }

    /// Adds `other` when it has the same shift. One with another shift comes from a different layout and is dropped.
    pub fn merge(&mut self, other: &Histogram) {
        if other.shift != self.shift {
            return;
        }
        for (into, from) in self.buckets.iter_mut().zip(other.buckets) {
            *into = into.saturating_add(from);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self.sum.saturating_add(other.sum);
    }
}

/// A count as its bit length: 0 for 0, 1 for 1, 2 for 2 and 3, 3 for 4 to 7, and so on. Reports send this instead of
/// an exact number that could tell one setup from another.
pub fn magnitude(value: u64) -> u8 {
    (64 - value.leading_zeros()) as u8
}

/// Something that happened once, sent on its own rather than summed into the day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A step of `koi setup` or `koi server ...` ended.
    SetupStep {
        day: u32,
        flow: SetupFlow,
        step: SetupStep,
        outcome: Outcome,
        /// Cloudflare's numeric API error code, when a Cloudflare call failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cloudflare_code: Option<u32>,
        /// The step's duration in seconds, as a [`magnitude`].
        seconds_magnitude: u8,
    },
    /// The process panicked here. The panic's message is never kept, since it can hold a path.
    Panic {
        day: u32,
        file: SourceFile,
        line: u32,
    },
}

impl Event {
    pub fn day(&self) -> u32 {
        match self {
            Event::SetupStep { day, .. } | Event::Panic { day, .. } => *day,
        }
    }

    /// The events of one kind an install keeps per day; later ones are dropped, so a loop cannot fill the queue.
    pub const PER_KIND_PER_DAY: usize = 20;

    pub fn kind(&self) -> &'static str {
        match self {
            Event::SetupStep { .. } => "setup_step",
            Event::Panic { .. } => "panic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupFlow {
    /// `koi setup`, logging in with Cloudflare.
    Cloudflare,
    /// `koi setup`, entering a pairing code.
    Pairing,
    Join,
    Deploy,
    AddDevice,
    Claim,
    Upgrade,
    Uninstall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    Login,
    R2Checkout,
    Bucket,
    Script,
    Serving,
    Claim,
    Pair,
    Upgrade,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Failed,
    /// The person stopped it, for example with Ctrl-C or by answering no.
    Aborted,
}

/// A daily sum: a metric and the label it is counted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    /// A CLI command ran. Only its name; never its arguments.
    Command(Command),
}

impl Counter {
    /// Every counter, for checking the keys of a stored report.
    pub fn all() -> impl Iterator<Item = Counter> {
        Command::ALL.into_iter().map(Counter::Command)
    }

    pub(crate) fn key(self) -> (&'static str, &'static str) {
        match self {
            Counter::Command(command) => ("command", command.name()),
        }
    }
}

/// A daily distribution of durations or sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {}

impl Measure {
    pub fn all() -> impl Iterator<Item = Measure> {
        std::iter::empty()
    }

    /// The metric, the label and the histogram's shift.
    pub(crate) fn key(self) -> (&'static str, &'static str, u8) {
        match self {}
    }
}

/// The CLI's top-level commands, by name. A group such as `koi folder` counts once, whichever action it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Resolve,
    Setup,
    Join,
    Server,
    Space,
    Folder,
    Device,
    History,
    Service,
    Daemon,
    Telemetry,
}

impl Command {
    pub const ALL: [Command; 12] = [
        Command::Status,
        Command::Resolve,
        Command::Setup,
        Command::Join,
        Command::Server,
        Command::Space,
        Command::Folder,
        Command::Device,
        Command::History,
        Command::Service,
        Command::Daemon,
        Command::Telemetry,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Resolve => "resolve",
            Command::Setup => "setup",
            Command::Join => "join",
            Command::Server => "server",
            Command::Space => "space",
            Command::Folder => "folder",
            Command::Device => "device",
            Command::History => "history",
            Command::Service => "service",
            Command::Daemon => "daemon",
            Command::Telemetry => "telemetry",
        }
    }
}

/// Which program sends the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    CliDaemon,
    CliCommand,
    MacosApp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Build {
    Release,
    Dev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Linux,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Arm64,
    X86_64,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Distro {
    Alpine,
    Arch,
    Debian,
    Fedora,
    Nixos,
    Opensuse,
    Rhel,
    Ubuntu,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsVersion {
    pub major: u16,
    pub minor: u16,
}

/// 128 random bits as 32 lowercase hex characters. It is drawn once per profile and says nothing about the machine,
/// the account or the Koidrive device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstallId(String);

impl InstallId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
        InstallId(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InstallId {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let hex = value.len() == 32
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        hex.then_some(InstallId(value)).ok_or(Invalid("install id"))
    }
}

impl From<InstallId> for String {
    fn from(id: InstallId) -> String {
        id.0
    }
}

impl fmt::Display for InstallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An event's uuid, drawn once when its batch is first taken and sent again with every retry, so PostHog keeps one copy
/// of a batch it got twice. Random, in the UUID version 4 layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventUuid(String);

impl EventUuid {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(EventUuid(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventUuid {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let valid = value.len() == 36
            && value.bytes().enumerate().all(|(i, byte)| match i {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
            });
        valid
            .then_some(EventUuid(value))
            .ok_or(Invalid("event uuid"))
    }
}

impl From<EventUuid> for String {
    fn from(uuid: EventUuid) -> String {
        uuid.0
    }
}

/// A version number such as `0.1.0` or `0.1.0-dev+abc123`: a digit first, then at most 63 characters from
/// `0-9A-Za-z.+-`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    pub fn parse(value: &str) -> Result<Self, Invalid> {
        let valid = value.len() <= 64
            && value.starts_with(|c: char| c.is_ascii_digit())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'));
        valid
            .then(|| Version(value.to_owned()))
            .ok_or(Invalid("version"))
    }
}

impl TryFrom<String> for Version {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        Version::parse(&value)
    }
}

impl From<Version> for String {
    fn from(version: Version) -> String {
        version.0
    }
}

/// A file of Koidrive's own source, as the compiler names it in a panic's location: `core/...` or `cli/...`, relative
/// to the repository. Any other location, such as a dependency's file under the home directory, becomes `external`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceFile(String);

impl SourceFile {
    pub const EXTERNAL: &'static str = "external";

    pub fn from_location(file: &str) -> Self {
        let ours = (file.starts_with("core/") || file.starts_with("cli/"))
            && file.ends_with(".rs")
            && !file.contains("..")
            && file.len() <= 128
            && file
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/'));
        SourceFile(if ours { file } else { Self::EXTERNAL }.to_owned())
    }
}

impl TryFrom<String> for SourceFile {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let file = SourceFile::from_location(&value);
        (file.0 == value)
            .then_some(file)
            .ok_or(Invalid("source file"))
    }
}

impl From<SourceFile> for String {
    fn from(file: SourceFile) -> String {
        file.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid {0}")]
pub struct Invalid(&'static str);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_puts_each_value_in_its_power_of_two_bucket() {
        let mut histogram = Histogram::new(5);
        for value in [0, 31, 32, 63, 64, 1 << 18, 1 << 19, u64::MAX] {
            histogram.record(value);
        }
        let mut expected = [0u64; 16];
        expected[0] = 2;
        expected[1] = 2;
        expected[2] = 1;
        expected[14] = 1;
        expected[15] = 2;
        assert_eq!(histogram.buckets, expected);
        assert_eq!(histogram.count, 8);
        assert_eq!(histogram.sum, u64::MAX);
    }

    #[test]
    fn histograms_with_another_shift_do_not_merge() {
        let mut ms = Histogram::new(5);
        ms.record(100);
        let mut other = Histogram::new(10);
        other.record(100);
        ms.merge(&other);
        assert_eq!(ms.count, 1);
        let same = ms.clone();
        ms.merge(&same);
        assert_eq!(ms.count, 2);
    }

    #[test]
    fn a_stored_report_keeps_only_known_keys() {
        let stored = serde_json::json!({
            "day": 100,
            "counters": {
                "command": { "status": 3, "/home/alice/private": 1 },
                "/home/alice/private": { "confidential.txt": 1 },
            },
            "histograms": {
                "/home/alice": { "notes.txt": { "shift": 0, "buckets": vec![0u64; 16], "count": 1, "sum": 1 } },
            },
        });
        let report: DailyReport = serde_json::from_value(stored).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({ "day": 100, "counters": { "command": { "status": 3 } } })
        );
    }

    #[test]
    fn every_command_is_listed_once() {
        let names: std::collections::BTreeSet<_> = Command::ALL.map(Command::name).into();
        assert_eq!(names.len(), Command::ALL.len());
    }

    #[test]
    fn magnitudes_are_bit_lengths() {
        assert_eq!(
            [0, 1, 2, 3, 4, 7, 8, 1000].map(magnitude),
            [0, 1, 2, 2, 3, 3, 4, 10]
        );
    }

    #[test]
    fn install_ids_are_32_random_hex_characters() {
        let (a, b) = (InstallId::generate(), InstallId::generate());
        assert_ne!(a, b);
        assert!(InstallId::try_from(a.as_str().to_owned()).is_ok());
        for bad in [
            "",
            "ABCDEF0123456789abcdef0123456789",
            "/home/alice/some/thing/0123456789",
        ] {
            assert!(InstallId::try_from(bad.to_owned()).is_err(), "{bad}");
        }
    }

    #[test]
    fn event_uuids_are_random_version_4_uuids() {
        let (a, b) = (
            EventUuid::generate().unwrap(),
            EventUuid::generate().unwrap(),
        );
        assert_ne!(a, b);
        assert_eq!(&a.as_str()[14..15], "4");
        assert!(EventUuid::try_from(a.as_str().to_owned()).is_ok());
        assert!(EventUuid::try_from("/home/alice/notes.txt/0123456789abcdef".to_owned()).is_err());
    }

    #[test]
    fn versions_refuse_anything_but_a_version_number() {
        for good in ["0.1.0", "0.1.0-dev+abc123", "14"] {
            assert!(Version::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "v0.1", "0.1 beta", "0.1/../x", "1.0\n"] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_panic_location_outside_the_source_tree_is_external() {
        assert_eq!(
            SourceFile::from_location("core/driver/src/folder.rs").0,
            "core/driver/src/folder.rs"
        );
        for outside in [
            "/Users/alice/.cargo/registry/src/index.crates.io-1/tokio-1.48.0/src/lib.rs",
            "core/../../Users/alice/x.rs",
            "core/Alice Notes.rs",
            "src/main.rs",
        ] {
            assert_eq!(
                SourceFile::from_location(outside).0,
                SourceFile::EXTERNAL,
                "{outside}"
            );
        }
        assert!(SourceFile::try_from("/Users/alice/x.rs".to_owned()).is_err());
    }
}
