use std::env;
use std::fmt;
use std::time::Duration;
use std::str::FromStr;
use std::sync::Arc;
use std::num::NonZeroU8;
use arrayvec::ArrayString;
use reqwest::{StatusCode, header};
use url::Url;
use tokio::time;
use tokio::sync::{mpsc, oneshot};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, NoneAsEmptyString, DurationSeconds, DurationMilliSeconds, DisplayFromStr, SpaceSeparator, StringWithSeparator};
use serde_repr::Deserialize_repr as DeserializeRepr;
use shakmaty::fen::Fen;
use shakmaty::uci::Uci;
use shakmaty::variant::Variant;
use crate::assets::EvalFlavor;
use crate::configure::{Endpoint, Key, KeyError};
use crate::logger::Logger;
use crate::util::{NevermindExt as _, RandomizedBackoff};

pub fn channel(endpoint: Endpoint, key: Option<Key>, logger: Logger) -> (ApiStub, ApiActor) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ApiStub { tx, endpoint: endpoint.clone() }, ApiActor::new(rx, endpoint, key, logger))
}

pub fn spawn(endpoint: Endpoint, key: Option<Key>, logger: Logger) -> ApiStub {
    let (stub, actor) = channel(endpoint, key, logger);
    tokio::spawn(async move {
        actor.run().await;
    });
    stub
}

#[derive(Debug)]
enum ApiMessage {
    CheckKey {
        callback: oneshot::Sender<Result<(), KeyError>>,
    },
    Status {
        callback: oneshot::Sender<AnalysisStatus>,
    },
    Abort {
        batch_id: BatchId,
    },
    Acquire {
        query: AcquireQuery,
        callback: oneshot::Sender<Acquired>,
    },
    SubmitAnalysis {
        batch_id: BatchId,
        flavor: EvalFlavor,
        analysis: Vec<Option<AnalysisPart>>,
    },
    SubmitMove {
        batch_id: BatchId,
        best_move: Option<Uci>,
        callback: oneshot::Sender<Acquired>,
    }
}

#[derive(Debug, Deserialize)]
struct StatusResponseBody {
    analysis: AnalysisStatus,
}

#[derive(Debug, Default, Deserialize)]
pub struct AnalysisStatus {
    pub user: QueueStatus,
    pub system: QueueStatus,
}

#[serde_as]
#[derive(Debug, Default, Deserialize)]
pub struct QueueStatus {
    // Using signed types here, because lila computes these values as
    // differences of non-atomic measurements. The results may occasionally be
    // negative.
    pub acquired: i64,
    pub queued: i64,
    #[serde_as(as = "DurationSeconds<u64>")]
    pub oldest: Duration,
}

#[derive(Debug, Serialize)]
pub struct VoidRequestBody {
    fishnet: Fishnet,
}

#[derive(Debug, Serialize)]
struct Fishnet {
    version: &'static str,
    apikey: String,
}

impl Fishnet {
    fn authenticated(key: Option<Key>) -> Fishnet {
        Fishnet {
            version: env!("CARGO_PKG_VERSION"),
            apikey: key.map_or("".to_owned(), |k| k.0),
        }
    }
}

#[derive(Debug, Serialize)]
struct Stockfish {
    flavor: EvalFlavor,
}

#[derive(Debug, Serialize)]
pub struct AcquireQuery {
    pub slow: bool,
}

#[serde_as]
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum Work {
    #[serde(rename = "analysis")]
    Analysis {
        #[serde_as(as = "DisplayFromStr")]
        id: BatchId,
        nodes: NodeLimit,
        #[serde(default)]
        depth: Option<u8>,
        #[serde(default)]
        multipv: Option<NonZeroU8>,
        #[serde_as(as = "Option<DurationMilliSeconds<u64>>")]
        #[serde(default)]
        timeout: Option<Duration>,
    },
    #[serde(rename = "move")]
    Move {
        #[serde_as(as = "DisplayFromStr")]
        id: BatchId,
        level: SkillLevel,
        #[serde(default)]
        clock: Option<Clock>,
    },
}

impl Work {
    pub fn id(&self) -> BatchId {
        match *self {
            Work::Analysis { id, .. } | Work::Move { id, .. } => id,
        }
    }

    pub fn is_analysis(&self) -> bool {
        matches!(self, Work::Analysis { .. })
    }

    pub fn multipv(&self) -> NonZeroU8 {
        match *self {
            Work::Analysis { multipv, .. } => multipv,
            Work::Move { .. } => None,
        }.unwrap_or_else(|| NonZeroU8::new(1).unwrap())
    }

    pub fn matrix_wanted(&self) -> bool {
        matches!(*self, Work::Analysis { multipv: Some(_), .. })
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct BatchId(ArrayString<24>);

impl FromStr for BatchId {
    type Err = arrayvec::CapacityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(BatchId(s.parse()?))
    }
}

impl fmt::Display for BatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Copy, Clone, Deserialize)]
pub struct NodeLimit {
    classical: u64,
    #[serde(default)]
    sf15: Option<u64>,
}

impl NodeLimit {
    pub fn get(&self, flavor: EvalFlavor) -> u64 {
        match flavor {
            EvalFlavor::Hce => self.classical,
            EvalFlavor::Nnue => self.sf15.unwrap_or(1_500_000),
        }
    }
}

#[derive(DeserializeRepr, Debug, Copy, Clone)]
#[repr(u32)]
pub enum SkillLevel {
    One = 1,
    Two = 2,
    Three = 3,
    Four = 4,
    Five = 5,
    Six = 6,
    Seven = 7,
    Eight = 8,
}

impl SkillLevel {
    pub fn time(self) -> Duration {
        use SkillLevel::*;
        Duration::from_millis(match self {
            One => 50,
            Two => 100,
            Three => 150,
            Four => 200,
            Five => 300,
            Six => 400,
            Seven => 500,
            Eight => 1000,
        })
    }

    pub fn skill_level(self) -> i32 {
        use SkillLevel::*;
        match self {
            One => -9,
            Two => -5,
            Three => -1,
            Four => 3,
            Five => 7,
            Six => 11,
            Seven => 16,
            Eight => 20,
        }
    }

    pub fn depth(self) -> u8 {
        use SkillLevel::*;
        match self {
            One | Two | Three | Four | Five => 5,
            Six => 8,
            Seven => 13,
            Eight => 22,
        }
    }
}

#[serde_as]
#[derive(Debug, Deserialize, Clone)]
pub struct Clock {
    pub wtime: Centis,
    pub btime: Centis,
    #[serde_as(as = "DurationSeconds<u64>")]
    pub inc: Duration,
}

#[derive(Debug, Copy, Clone, Deserialize)]
pub struct Centis(u32);

impl From<Centis> for Duration {
    fn from(Centis(centis): Centis) -> Duration {
        Duration::from_millis(u64::from(centis) * 10)
    }
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct AcquireResponseBody {
    pub work: Work,
    #[serde_as(as = "NoneAsEmptyString")]
    #[serde(default)]
    pub game_id: Option<String>,
    #[serde_as(as = "DisplayFromStr")]
    pub position: Fen,
    #[serde(default)]
    pub variant: LichessVariant,
    #[serde_as(as = "StringWithSeparator::<SpaceSeparator, Uci>")]
    pub moves: Vec<Uci>,
    #[serde(rename = "skipPositions", default)]
    pub skip_positions: Vec<usize>,
}

impl AcquireResponseBody {
    pub fn batch_url(&self, endpoint: &Endpoint) -> Option<Url> {
        self.game_id.as_ref().map(|g| {
            let mut url = endpoint.url.clone();
            url.set_path(g);
            url
        })
    }
}

#[derive(Debug, Deserialize, Copy, Clone, Eq, PartialEq)]
pub enum LichessVariant {
    #[serde(rename = "antichess")]
    Antichess,
    #[serde(rename = "atomic")]
    Atomic,
    #[serde(rename = "chess960")]
    Chess960,
    #[serde(rename = "crazyhouse")]
    Crazyhouse,
    #[serde(rename = "fromPosition")]
    FromPosition,
    #[serde(rename = "horde")]
    Horde,
    #[serde(rename = "kingOfTheHill")]
    KingOfTheHill,
    #[serde(rename = "racingKings")]
    RacingKings,
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "threeCheck")]
    ThreeCheck,
}

impl LichessVariant {
    pub fn short_name(self) -> Option<&'static str> {
        Some(match self {
            LichessVariant::Antichess => "anti",
            LichessVariant::Atomic => "atomic",
            LichessVariant::Chess960 => "chess960",
            LichessVariant::Crazyhouse => "zh",
            LichessVariant::FromPosition => "setup",
            LichessVariant::Horde => "horde",
            LichessVariant::KingOfTheHill => "koth",
            LichessVariant::RacingKings => "race",
            LichessVariant::ThreeCheck => "3check",
            LichessVariant::Standard => return None,
        })
    }
}

impl From<LichessVariant> for Variant {
    fn from(lichess: LichessVariant) -> Variant {
        match lichess {
            LichessVariant::Antichess => Variant::Antichess,
            LichessVariant::Atomic => Variant::Atomic,
            LichessVariant::Chess960 | LichessVariant::Standard | LichessVariant::FromPosition => Variant::Chess,
            LichessVariant::Crazyhouse => Variant::Crazyhouse,
            LichessVariant::Horde => Variant::Horde,
            LichessVariant::KingOfTheHill => Variant::KingOfTheHill,
            LichessVariant::RacingKings => Variant::RacingKings,
            LichessVariant::ThreeCheck => Variant::ThreeCheck,
        }
    }
}

impl Default for LichessVariant {
    fn default() -> LichessVariant {
        LichessVariant::Standard
    }
}

#[must_use = "Acquired work should be processed or cancelled"]
#[derive(Debug)]
pub enum Acquired {
    Accepted(AcquireResponseBody),
    NoContent,
    Rejected,
}

#[derive(Debug, Serialize)]
struct AnalysisRequestBody {
    fishnet: Fishnet,
    stockfish: Stockfish,
    analysis: Vec<Option<AnalysisPart>>,
}

#[derive(Debug, Serialize)]
struct MoveRequestBody {
    fishnet: Fishnet,
    #[serde(rename = "move")]
    m: BestMove,
}

#[serde_as]
#[derive(Debug, Serialize)]
struct BestMove {
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(rename = "bestmove")]
    best_move: Option<Uci>,
}

#[serde_as]
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AnalysisPart {
    Skipped {
        skipped: bool,
    },
    Best {
        #[serde_as(as = "StringWithSeparator::<SpaceSeparator, Uci>")]
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pv: Vec<Uci>,
        score: Score,
        depth: u8,
        nodes: u64,
        time: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        nps: Option<u32>,
    },
    Matrix {
        #[serde_as(as = "Vec<Vec<Option<Vec<DisplayFromStr>>>>")]
        pv: Vec<Vec<Option<Vec<Uci>>>>,
        score: Vec<Vec<Option<Score>>>,
        depth: u8,
        nodes: u64,
        time: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        nps: Option<u32>,
    },
}

#[derive(Debug, Serialize, Copy, Clone)]
pub enum Score {
    #[serde(rename = "cp")]
    Cp(i64),
    #[serde(rename = "mate")]
    Mate(i64),
}

#[derive(Debug, Serialize)]
struct SubmitQuery {
    slow: bool,
    stop: bool,
}

#[derive(Debug, Clone)]
pub struct ApiStub {
    tx: mpsc::UnboundedSender<ApiMessage>,
    endpoint: Endpoint,
}

impl ApiStub {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub async fn check_key(&mut self) -> Option<Result<(), KeyError>> {
        let (req, res) = oneshot::channel();
        self.tx.send(ApiMessage::CheckKey {
            callback: req,
        }).expect("api actor alive");
        res.await.ok()
    }

    pub async fn status(&mut self) -> Option<AnalysisStatus> {
        let (req, res) = oneshot::channel();
        self.tx.send(ApiMessage::Status {
            callback: req,
        }).expect("api actor alive");
        res.await.ok()
    }

    pub fn abort(&mut self, batch_id: BatchId) {
        self.tx.send(ApiMessage::Abort { batch_id }).expect("api actor alive");
    }

    pub async fn acquire(&mut self, query: AcquireQuery) -> Option<Acquired> {
        let (req, res) = oneshot::channel();
        self.tx.send(ApiMessage::Acquire {
            query,
            callback: req,
        }).expect("api actor alive");
        res.await.ok()
    }

    pub fn submit_analysis(&mut self, batch_id: BatchId, flavor: EvalFlavor, analysis: Vec<Option<AnalysisPart>>) {
        self.tx.send(ApiMessage::SubmitAnalysis {
            batch_id,
            flavor,
            analysis,
        }).expect("api actor alive");
    }

    pub async fn submit_move_and_acquire(&mut self, batch_id: BatchId, best_move: Option<Uci>) -> Option<Acquired> {
        let (req, res) = oneshot::channel();
        self.tx.send(ApiMessage::SubmitMove {
            batch_id,
            best_move,
            callback: req,
        }).expect("api actor alive");
        res.await.ok()
    }
}

pub struct ApiActor {
    rx: mpsc::UnboundedReceiver<ApiMessage>,
    endpoint: Endpoint,
    key: Option<Key>,
    client: reqwest::Client,
    error_backoff: RandomizedBackoff,
    logger: Logger,
}

impl ApiActor {
    fn new(rx: mpsc::UnboundedReceiver<ApiMessage>, endpoint: Endpoint, key: Option<Key>, logger: Logger) -> ApiActor {
        // Build TLS backend that supports SSLKEYLOGFILE.
        let mut tls = rustls::ClientConfig::new();
        tls.set_protocols(&["h2".into(), "http/1.1".into()]);
        tls.root_store.add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        tls.key_log = Arc::new(rustls::KeyLogFile::new());

        let mut headers = header::HeaderMap::new();
        if let Some(Key(ref key)) = key {
            headers.insert(header::AUTHORIZATION, format!("Bearer {}", key).parse().expect("header value"));
        }

        ApiActor {
            rx,
            endpoint,
            key,
            client: reqwest::Client::builder()
                .default_headers(headers)
                .user_agent(format!("{}-{}-{}/{}", env!("CARGO_PKG_NAME"), env::consts::OS, env::consts::ARCH, env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(25))
                .use_preconfigured_tls(tls)
                .build().expect("client"),
            error_backoff: RandomizedBackoff::default(),
            logger,
        }
    }

    pub async fn run(mut self) {
        self.logger.debug("Api actor started");
        while let Some(msg) = self.rx.recv().await {
            self.handle_message(msg).await;
        }
        self.logger.debug("Api actor exited");
    }

    async fn handle_message(&mut self, msg: ApiMessage) {
        if let Err(err) = self.handle_message_inner(msg).await {
            if err.status().map_or(false, |s| s.is_success()) {
                self.error_backoff.reset();
            } else if err.status() == Some(StatusCode::TOO_MANY_REQUESTS) {
                let backoff = Duration::from_secs(60) + self.error_backoff.next();
                self.logger.error(&format!("Too many requests. Suspending requests for {:?}.", backoff));
                time::sleep(backoff).await;
            } else {
                let backoff = self.error_backoff.next();
                self.logger.error(&format!("{}. Backing off {:?}.", err, backoff));
                time::sleep(backoff).await;
            }
        } else {
            self.error_backoff.reset();
        }
    }

    async fn abort(&mut self, batch_id: BatchId) -> reqwest::Result<()> {
        let url = format!("{}/abort/{}", self.endpoint, batch_id);
        self.logger.warn(&format!("Aborting batch {}.", batch_id));
        let res = self.client.post(&url).json(&VoidRequestBody {
            fishnet: Fishnet::authenticated(self.key.clone()),
        }).send().await?;

        if res.status() == StatusCode::NOT_FOUND {
            self.logger.warn(&format!("Fishnet server does not support abort (404 for {}).", batch_id));
            Ok(())
        } else {
            res.error_for_status().map(|_| ())
        }
    }

    async fn handle_message_inner(&mut self, msg: ApiMessage) -> reqwest::Result<()> {
        match msg {
            ApiMessage::CheckKey { callback } => {
                let url = format!("{}/key", self.endpoint);
                let res = self.client.get(&url).send().await?;
                match res.status() {
                    StatusCode::NO_CONTENT | StatusCode::OK => {
                        callback.send(Ok(())).nevermind("callback dropped");
                    }
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                        callback.send(Err(KeyError::AccessDenied)).nevermind("callback dropped");
                    }
                    StatusCode::NOT_FOUND => {
                        // Legacy key validation.
                        self.logger.debug("Falling back to legacy key validation");
                        let url = format!("{}/key/{}", self.endpoint, self.key.as_ref().map_or("", |k| &k.0));
                        let res = self.client.get(&url).send().await?;
                        match res.status() {
                            StatusCode::NOT_FOUND => callback.send(Err(KeyError::AccessDenied)).nevermind("callback dropped"),
                            StatusCode::OK => callback.send(Ok(())).nevermind("callback dropped"),
                            status => {
                                self.logger.warn(&format!("Unexpected status while checking legacy key: {}", status));
                                res.error_for_status()?;
                            }
                        }
                    }
                    status => {
                        self.logger.warn(&format!("Unexpected status while checking key: {}", status));
                        res.error_for_status()?;
                    }
                }
            }
            ApiMessage::Status { callback } => {
                let url = format!("{}/status", self.endpoint);
                let res = self.client.get(&url).send().await?;
                match res.status() {
                    StatusCode::OK => callback.send(res.json::<StatusResponseBody>().await?.analysis).nevermind("callback dropped"),
                    StatusCode::NOT_FOUND => (),
                    status => {
                        self.logger.warn(&format!("Unexpected status for queue status: {}", status));
                        res.error_for_status()?;
                    }
                }
            }
            ApiMessage::Abort { batch_id } => {
                self.abort(batch_id).await?;
            }
            ApiMessage::Acquire { callback, query } => {
                let url = format!("{}/acquire", self.endpoint);
                let res = self.client.post(&url).query(&query).json(&VoidRequestBody {
                    fishnet: Fishnet::authenticated(self.key.clone()),
                }).send().await?;

                match res.status() {
                    StatusCode::NO_CONTENT => callback.send(Acquired::NoContent).nevermind("callback dropped"),
                    StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_ACCEPTABLE => {
                        let text = res.text().await?;
                        self.logger.error(&format!("Server rejected request: {}", text));
                        callback.send(Acquired::Rejected).nevermind("callback dropped");
                    }
                    StatusCode::OK | StatusCode::ACCEPTED => {
                        if let Err(Acquired::Accepted(res)) = callback.send(Acquired::Accepted(res.json().await?)) {
                            self.logger.error("Acquired a batch, but callback dropped. Aborting.");
                            self.abort(res.work.id()).await?;
                        }
                    }
                    status => {
                        self.logger.warn(&format!("Unexpected status for acquire: {}", status));
                        res.error_for_status()?;
                    }
                }
            }
            ApiMessage::SubmitAnalysis { batch_id, flavor, analysis } => {
                let url = format!("{}/analysis/{}", self.endpoint, batch_id);
                let res = self.client.post(&url).query(&SubmitQuery {
                    stop: true,
                    slow: false,
                }).json(&AnalysisRequestBody {
                    fishnet: Fishnet::authenticated(self.key.clone()),
                    stockfish: Stockfish { flavor },
                    analysis,
                }).send().await?.error_for_status()?;

                if res.status() != StatusCode::NO_CONTENT {
                    self.logger.warn(&format!("Unexpected status for submitting analysis: {}", res.status()));
                }
            }
            ApiMessage::SubmitMove { batch_id, best_move, callback } => {
                let url = format!("{}/move/{}", self.endpoint, batch_id);
                let res = self.client.post(&url).json(&MoveRequestBody {
                    fishnet: Fishnet::authenticated(self.key.clone()),
                    m: BestMove {
                        best_move: best_move.clone(),
                    },
                }).send().await?;

                match res.status() {
                    StatusCode::NO_CONTENT => callback.send(Acquired::NoContent).nevermind("callback dropped"),
                    StatusCode::OK | StatusCode::ACCEPTED => {
                        if let Err(Acquired::Accepted(res)) = callback.send(Acquired::Accepted(res.json().await?)) {
                            self.logger.error("Acquired a batch while submitting move, but callback dropped. Aborting.");
                            self.abort(res.work.id()).await?;
                        }
                    }
                    status => {
                        self.logger.warn(&format!("Unexpected status submitting move {} for batch {}: {}",
                                                  best_move.unwrap_or(Uci::Null),
                                                  batch_id, status));
                        res.error_for_status()?;
                    }
                }
            }
        }

        Ok(())
    }
}
