//! The compile pipeline behind `mac_programs.rs`, portable (no Metal) so it is unit-tested on Linux with a fake
//! backend:
//!
//! - [`Compiler`]: a worker pool that compiles stage libraries and pipeline states off the render thread. Identical
//!   stage sources are compiled once (keyed by a hash of the MSL). The render thread adds programs, requests
//!   pipelines and polls for [`Event`]s; it only blocks where it asks to ([`Compiler::wait_pipeline`]).
//! - [`Manifest`]: every (program, pipeline key) a run really used, persisted next to the stage sources
//!   (`<dir>/manifest.json`, `<dir>/stages/<hash>.metal`). The next launch compiles all of it in the background
//!   before the game asks ("prewarm"), from the stored MSL: no translation, no JS needed.
//! - [`PipelineBackend`]: what turns MSL into a library and a [`PipelineJob`] into a pipeline state, and persists
//!   its own binary form. Metal 3 today (`mac_programs::Metal3`: `newLibraryWithSource`,
//!   `newRenderPipelineState`, `MTLBinaryArchive`); the Metal 4 port (WP M-E) implements the same three methods
//!   with `MTL4Compiler`, `lookupArchives` and `MTL4PipelineDataSetSerializer` -> `MTL4Archive`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Hash of a stage's MSL source, or of a program (its two stage hashes and entry points).
pub type Hash = u128;

/// Bumped when the meaning of a manifest entry changes (stream signature bits, target keys, attachment formats):
/// an older manifest is ignored.
pub const MANIFEST_VERSION: u32 = 1;
/// Entries not used for this many launches are dropped (and no longer prewarmed).
pub const KEEP_RUNS: u64 = 8;

/// 128-bit hash of `parts` (length-prefixed, so part boundaries count). Not cryptographic: a cache key.
pub fn hash128(parts: &[&[u8]]) -> Hash {
    fn mix(mut h: u64, v: u64) -> u64 {
        h = (h ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 29;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^ (h >> 32)
    }
    let (mut a, mut b) = (0x243F_6A88_85A3_08D3u64, 0x1319_8A2E_0370_7344u64);
    for p in parts {
        a = mix(a, p.len() as u64);
        b = mix(b, !(p.len() as u64));
        let mut chunks = p.chunks_exact(8);
        for c in &mut chunks {
            let v = u64::from_le_bytes(c.try_into().unwrap());
            a = mix(a, v);
            b = mix(b.rotate_left(23), v ^ 0xA5A5_A5A5_A5A5_A5A5);
        }
        let mut tail = [0u8; 8];
        let rest = chunks.remainder();
        tail[..rest.len()].copy_from_slice(rest);
        let v = u64::from_le_bytes(tail);
        a = mix(a, v);
        b = mix(b.rotate_left(23), v ^ 0xA5A5_A5A5_A5A5_A5A5);
    }
    // mix is symmetric in its arguments: finalise each half through its own constant first
    ((mix(mix(a, 0x452821E638D01377), b) as u128) << 64) | mix(mix(b, 0xBE5466CF34E90C6C), a) as u128
}

pub fn hex(h: Hash) -> String {
    format!("{h:032x}")
}

mod hex128 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

/// What a pipeline renders into.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Target {
    /// the main pass: the output colour format + Depth32Float
    Main,
    /// a shadow map: Depth32Float only (three's depth programs; their colour output is discarded)
    Depth,
    /// a VSM blur pass: RG16Float colour, no depth
    Rg16,
    /// a render target (src/mac_targets.rs): its attachment formats as `three::target_key` packs them
    Rt(u32),
}

/// One vertex input of a program, as the vertex descriptor needs it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Attr {
    pub location: u32,
    /// float components the shader reads (1..4); for a matrix: rows
    pub comps: u32,
    /// 4 for a mat4 attribute, else 1
    pub cols: u32,
}

/// A program as the compiler knows it: two stages by source hash. `hash` identifies it across launches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProgramInfo {
    /// three's program name (logs only)
    pub name: String,
    #[serde(with = "hex128")]
    pub vertex: Hash,
    #[serde(with = "hex128")]
    pub fragment: Hash,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub attrs: Vec<Attr>,
}

impl ProgramInfo {
    pub fn new(name: &str, vertex_msl: &str, fragment_msl: &str, vertex_entry: &str, fragment_entry: &str, attrs: Vec<Attr>) -> Self {
        ProgramInfo {
            name: name.to_string(),
            vertex: hash128(&[vertex_msl.as_bytes()]),
            fragment: hash128(&[fragment_msl.as_bytes()]),
            vertex_entry: vertex_entry.to_string(),
            fragment_entry: fragment_entry.to_string(),
            attrs,
        }
    }

    pub fn hash(&self) -> Hash {
        hash128(&[&self.vertex.to_le_bytes(), &self.fragment.to_le_bytes(), self.vertex_entry.as_bytes(), self.fragment_entry.as_bytes()])
    }
}

/// Everything that selects a pipeline state, stable across launches (the program by hash, not by this run's id).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct PipelineKey {
    #[serde(with = "hex128")]
    pub program: Hash,
    /// three's blend constants: [src rgb, dst rgb, src alpha, dst alpha, equation rgb, equation alpha]
    pub blend: Option<[u32; 6]>,
    pub color_write: bool,
    /// 4 bits per attribute (`mac_programs::StreamSig`)
    pub sig: u64,
    pub target: Target,
}

// ------------------------------------------------------------------------------------------------------- manifest

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ProgramRec {
    #[serde(flatten)]
    info: ProgramInfo,
    last_run: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PipelineRec {
    #[serde(flatten)]
    key: PipelineKey,
    last_run: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct ManifestFile {
    version: u32,
    run: u64,
    programs: Vec<ProgramRec>,
    pipelines: Vec<PipelineRec>,
}

/// The programs and pipelines recent launches used. In memory: maps; on disk: `<dir>/manifest.json`.
#[derive(Default, Clone)]
pub struct Manifest {
    run: u64,
    programs: HashMap<Hash, (ProgramInfo, u64)>,
    pipelines: HashMap<PipelineKey, u64>,
    dirty: bool,
}

impl Manifest {
    /// Last launch's manifest (empty when there is none, or it is unreadable or of another version), with this
    /// launch's run number and without the entries no launch used for [`KEEP_RUNS`] runs.
    pub fn load(dir: &Path) -> Manifest {
        let f: ManifestFile = std::fs::read(dir.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(|f: &ManifestFile| f.version == MANIFEST_VERSION).unwrap_or_default();
        let run = f.run + 1;
        let live = |last: u64| last + KEEP_RUNS >= run;
        let programs: HashMap<Hash, (ProgramInfo, u64)> = f.programs.into_iter().filter(|p| live(p.last_run)).map(|p| (p.info.hash(), (p.info, p.last_run))).collect();
        let pipelines = f.pipelines.into_iter().filter(|p| live(p.last_run) && programs.contains_key(&p.key.program)).map(|p| (p.key, p.last_run)).collect();
        Manifest { run, programs, pipelines, dirty: false }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let mut f = ManifestFile { version: MANIFEST_VERSION, run: self.run, programs: Vec::new(), pipelines: Vec::new() };
        f.programs = self.programs.values().map(|(info, last_run)| ProgramRec { info: info.clone(), last_run: *last_run }).collect();
        f.programs.sort_by_key(|p| p.info.hash());
        f.pipelines = self.pipelines.iter().map(|(key, last_run)| PipelineRec { key: *key, last_run: *last_run }).collect();
        f.pipelines.sort_by_key(|p| (p.key.program, p.key.sig, format!("{:?}{:?}{}", p.key.target, p.key.blend, p.key.color_write)));
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("manifest.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&f).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, dir.join("manifest.json"))
    }

    pub fn use_program(&mut self, info: &ProgramInfo) {
        let e = self.programs.entry(info.hash()).or_insert_with(|| (info.clone(), 0));
        if e.1 != self.run {
            e.1 = self.run;
            self.dirty = true;
        }
    }

    pub fn use_pipeline(&mut self, key: &PipelineKey) {
        let e = self.pipelines.entry(*key).or_insert(0);
        if *e != self.run {
            *e = self.run;
            self.dirty = true;
        }
    }

    pub fn programs(&self) -> impl Iterator<Item = &ProgramInfo> { self.programs.values().map(|p| &p.0) }
    pub fn pipelines(&self) -> impl Iterator<Item = &PipelineKey> { self.pipelines.keys() }
    pub fn len(&self) -> (usize, usize) { (self.programs.len(), self.pipelines.len()) }
}

/// `$FASTRENDER_PIPELINE_CACHE` (`off` / `0` / empty: nothing is persisted or prewarmed), else
/// `~/Library/Caches/fastrender/pipelines` (macOS), `$XDG_CACHE_HOME/fastrender/pipelines` or
/// `~/.cache/fastrender/pipelines`. The base directory: `mac_programs` keeps one sub-directory per game (a
/// manifest prewarms everything in it, so it should hold one game's pipelines).
pub fn default_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("FASTRENDER_PIPELINE_CACHE") {
        return (!matches!(d.as_str(), "" | "0" | "off")).then(|| PathBuf::from(d));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Caches/fastrender/pipelines"));
    }
    Some(match std::env::var_os("XDG_CACHE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x).join("fastrender/pipelines"),
        _ => home.join(".cache/fastrender/pipelines"),
    })
}

fn stage_path(dir: &Path, h: Hash) -> PathBuf {
    dir.join("stages").join(format!("{}.metal", hex(h)))
}

/// A stage's MSL as an earlier launch stored it (None: missing, or not the source this hash names).
fn read_stage(dir: &Path, h: Hash) -> Option<Arc<str>> {
    let s = std::fs::read_to_string(stage_path(dir, h)).ok()?;
    (hash128(&[s.as_bytes()]) == h).then(|| Arc::from(s))
}

fn write_stage(dir: &Path, h: Hash, src: &str) -> std::io::Result<()> {
    let p = stage_path(dir, h);
    if p.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir.join("stages"))?;
    let tmp = dir.join("stages").join(format!("{}.{}.tmp", hex(h), std::process::id()));
    std::fs::write(&tmp, src)?;
    std::fs::rename(&tmp, p)
}

/// Stage files of earlier launches (older than `before`) no manifest program refers to any more (aged out), and
/// temporary files a crash left. Files this launch wrote stay: their program may not be in `m` yet.
fn tidy_stages(dir: &Path, m: &Manifest, before: std::time::SystemTime) {
    let live: std::collections::HashSet<String> = m.programs().flat_map(|p| [hex(p.vertex), hex(p.fragment)]).collect();
    let Ok(rd) = std::fs::read_dir(dir.join("stages")) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let keep = name.strip_suffix(".metal").is_some_and(|k| live.contains(k));
        let old = e.metadata().and_then(|md| md.modified()).is_ok_and(|t| t < before);
        if !keep && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// -------------------------------------------------------------------------------------------------------- backend

/// One pipeline to build: both stages compiled, the vertex inputs, and the key (streams, blend, target).
pub struct PipelineJob<'a, L> {
    pub vertex: &'a L,
    pub vertex_entry: &'a str,
    pub fragment: &'a L,
    pub fragment_entry: &'a str,
    pub attrs: &'a [Attr],
    pub key: &'a PipelineKey,
}

/// The graphics API side of the compiler. Every method is called on compile workers, several at once.
pub trait PipelineBackend: Send + Sync + 'static {
    /// a compiled stage library
    type Library: Clone + Send + 'static;
    type Pipeline: Clone + Send + 'static;
    fn library(&self, msl: &str) -> Result<Self::Library, String>;
    /// The pipeline state, and whether it came out of the persisted archive (true) or was compiled (false). A
    /// lookup miss must fall back to compiling.
    fn pipeline(&self, job: &PipelineJob<'_, Self::Library>) -> Result<(Self::Pipeline, bool), String>;
    /// Write what `pipeline` compiled since the last call into the persisted archive. Returns a line for the log
    /// (None: nothing to do, or no archive).
    fn persist(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

// ------------------------------------------------------------------------------------------------------- compiler

/// What the render thread learns from [`Compiler::poll`].
pub enum Event<P> {
    /// both stage libraries of an added program compiled (or one failed)
    Program { hash: Hash, result: Result<(), String> },
    /// a requested pipeline is ready (or failed)
    Pipeline { key: PipelineKey, result: Result<P, String> },
    /// a persist finished: the backend's log line or error
    Persisted(Result<Option<String>, String>),
}

/// Answer to [`Compiler::request`].
pub enum Status<P> {
    Ready(P),
    /// queued or compiling: an [`Event::Pipeline`] follows
    Pending,
    Failed(String),
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// stage libraries compiled, and their worker time
    pub libraries: usize,
    pub library_ms: f64,
    /// stages of added programs that an earlier program (or the manifest) had already brought: not compiled again
    pub libraries_shared: usize,
    /// pipelines built (compiled or from the archive), and their worker time
    pub pipelines: usize,
    pub pipeline_ms: f64,
    pub archive_hits: usize,
    /// programs / pipelines of the manifest compiled before anything asked for them
    pub prewarmed_programs: usize,
    pub prewarmed_pipelines: usize,
    /// manifest entries at launch
    pub manifest_programs: usize,
    pub manifest_pipelines: usize,
    /// added programs / requested pipelines still compiling
    pub pending_programs: usize,
    pub pending_pipelines: usize,
    /// manifest jobs not yet run
    pub prewarm_queued: usize,
}

enum Lib<L> {
    Compiling,
    Ready(L),
    Failed(String),
}

enum Work<T> {
    Queued,
    Building,
    Done(Result<T, String>),
}

struct Entry<T> {
    work: Work<T>,
    /// the render thread asked for it: completion is an event
    wanted: bool,
}

#[derive(Clone, Copy)]
enum Job {
    /// one stage library, so a program's two stages compile on two workers
    Stage(Hash),
    Program(Hash),
    Pipeline(PipelineKey),
    Persist,
}

struct State<B: PipelineBackend> {
    demand: VecDeque<Job>,
    prewarm: VecDeque<Job>,
    sources: HashMap<Hash, Arc<str>>,
    libs: HashMap<Hash, Lib<B::Library>>,
    infos: HashMap<Hash, Arc<ProgramInfo>>,
    programs: HashMap<Hash, Entry<()>>,
    pipes: HashMap<PipelineKey, Entry<B::Pipeline>>,
    events: Vec<Event<B::Pipeline>>,
    manifest: Manifest,
    running: usize,
    persisting: bool,
    shutdown: bool,
    stats: Stats,
}

struct Shared<B: PipelineBackend> {
    backend: B,
    dir: Option<PathBuf>,
    started: std::time::SystemTime,
    state: Mutex<State<B>>,
    /// workers wait here for jobs
    work: Condvar,
    /// anything finished: waiting workers (a library another worker compiles) and the render thread
    done: Condvar,
}

/// Why a stage could not be compiled.
enum LibError {
    /// the manifest names a stage whose source is not on disk (and no program brought it yet): not a failure
    Missing,
    Failed(String),
}

pub struct Compiler<B: PipelineBackend> {
    shared: Arc<Shared<B>>,
}

impl<B: PipelineBackend> Drop for Compiler<B> {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.work.notify_all();
    }
}

impl<B: PipelineBackend> Shared<B> {
    fn lock(&self) -> MutexGuard<'_, State<B>> { self.state.lock().unwrap_or_else(|e| e.into_inner()) }

    fn wait<'a>(&self, g: MutexGuard<'a, State<B>>) -> MutexGuard<'a, State<B>> { self.done.wait(g).unwrap_or_else(|e| e.into_inner()) }

    /// A stage library, compiled once however many programs and workers ask.
    fn library(&self, h: Hash) -> Result<B::Library, LibError> {
        let src = {
            let mut st = self.lock();
            loop {
                match st.libs.get(&h) {
                    Some(Lib::Ready(l)) => return Ok(l.clone()),
                    Some(Lib::Failed(e)) => return Err(LibError::Failed(e.clone())),
                    // another worker has it; if that one finds no source (a manifest stage not on disk) the entry
                    // goes away and this request compiles it
                    Some(Lib::Compiling) => st = self.wait(st),
                    None => break,
                }
            }
            st.libs.insert(h, Lib::Compiling);
            st.sources.get(&h).cloned()
        };
        let from_memory = src.is_some();
        let Some(src) = src.or_else(|| self.dir.as_deref().and_then(|d| read_stage(d, h))) else {
            self.lock().libs.remove(&h);
            self.done.notify_all();
            return Err(LibError::Missing);
        };
        let t = Instant::now();
        let r = self.backend.library(&src);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if let (true, Some(d), true) = (from_memory, self.dir.as_deref(), r.is_ok()) {
            if let Err(e) = write_stage(d, h, &src) {
                eprintln!("pipeline cache: cannot store a stage in {}: {e}", d.display());
            }
        }
        let mut st = self.lock();
        st.stats.libraries += 1;
        st.stats.library_ms += ms;
        st.libs.insert(h, match &r { Ok(l) => Lib::Ready(l.clone()), Err(e) => Lib::Failed(e.clone()) });
        st.sources.remove(&h); // the library is what later requests get
        drop(st);
        self.done.notify_all();
        r.map_err(LibError::Failed)
    }

    fn stages(&self, info: &ProgramInfo) -> Result<(B::Library, B::Library), LibError> {
        let v = self.library(info.vertex).map_err(|e| e.label(&info.vertex_entry))?;
        let f = self.library(info.fragment).map_err(|e| e.label(&info.fragment_entry))?;
        Ok((v, f))
    }

    fn run_program(&self, h: Hash) {
        let info = {
            let mut st = self.lock();
            let Some(info) = st.infos.get(&h).cloned() else { return };
            match st.programs.get_mut(&h) {
                Some(e) if matches!(e.work, Work::Queued) => e.work = Work::Building,
                _ => return,
            }
            info
        };
        let r = self.stages(&info);
        let mut st = self.lock();
        let wanted = st.programs.get(&h).is_some_and(|e| e.wanted);
        match r {
            Err(LibError::Missing) if !wanted => {
                st.programs.remove(&h);
            }
            // the program arrived (with its sources) while the manifest's copy looked for them on disk: again
            Err(LibError::Missing) => {
                st.programs.insert(h, Entry { work: Work::Queued, wanted });
                st.demand.push_back(Job::Program(h));
                self.work.notify_one();
            }
            r => {
                let result = r.map(|_| ()).map_err(|e| e.message());
                if wanted {
                    st.stats.pending_programs -= 1;
                    st.events.push(Event::Program { hash: h, result: result.clone() });
                } else if result.is_ok() {
                    st.stats.prewarmed_programs += 1;
                }
                st.programs.insert(h, Entry { work: Work::Done(result), wanted });
            }
        }
    }

    fn run_pipeline(&self, key: PipelineKey) {
        let info = {
            let mut st = self.lock();
            let info = st.infos.get(&key.program).cloned();
            match st.pipes.get_mut(&key) {
                Some(e) if matches!(e.work, Work::Queued) => e.work = Work::Building,
                _ => return,
            }
            info
        };
        let mut built_ms = None;
        let r = match &info {
            None => Err(LibError::Failed("its program is not known".into())),
            Some(info) => self.stages(info).and_then(|(v, f)| {
                let t = Instant::now();
                let job = PipelineJob { vertex: &v, vertex_entry: &info.vertex_entry, fragment: &f, fragment_entry: &info.fragment_entry, attrs: &info.attrs, key: &key };
                let r = self.backend.pipeline(&job).map_err(LibError::Failed);
                built_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                r
            }),
        };
        let mut st = self.lock();
        let wanted = st.pipes.get(&key).is_some_and(|e| e.wanted);
        match r {
            Err(LibError::Missing) if !wanted => {
                st.pipes.remove(&key);
            }
            Err(LibError::Missing) => {
                st.pipes.insert(key, Entry { work: Work::Queued, wanted });
                st.demand.push_back(Job::Pipeline(key));
                self.work.notify_one();
            }
            r => {
                let result = r.map_err(|e| e.message());
                if let (Some(ms), Ok((_, hit))) = (built_ms, &result) {
                    st.stats.pipelines += 1;
                    st.stats.pipeline_ms += ms;
                    st.stats.archive_hits += *hit as usize;
                }
                let result = result.map(|(p, _)| p);
                if wanted {
                    st.stats.pending_pipelines -= 1;
                    st.events.push(Event::Pipeline { key, result: result.clone() });
                } else if result.is_ok() {
                    st.stats.prewarmed_pipelines += 1;
                }
                st.pipes.insert(key, Entry { work: Work::Done(result), wanted });
            }
        }
    }

    fn run_persist(&self) {
        let manifest = {
            let mut st = self.lock();
            let m = st.manifest.dirty.then(|| st.manifest.clone());
            st.manifest.dirty = false;
            m
        };
        let mut result = Ok(None);
        if let Some(dir) = self.dir.as_deref() {
            if let Some(m) = manifest {
                match m.save(dir) {
                    Ok(()) => tidy_stages(dir, &m, self.started),
                    Err(e) => result = Err(format!("manifest not written in {}: {e}", dir.display())),
                }
            }
            if result.is_ok() {
                result = self.backend.persist();
            }
        }
        let mut st = self.lock();
        st.persisting = false;
        st.events.push(Event::Persisted(result));
    }

    fn worker(&self) {
        loop {
            let job = {
                let mut st = self.lock();
                loop {
                    if st.shutdown {
                        return;
                    }
                    // `running` under the same lock as the pop: wait_idle never sees "empty and idle" in between
                    if let Some(j) = st.demand.pop_front() {
                        st.running += 1;
                        break j;
                    }
                    if let Some(j) = st.prewarm.pop_front() {
                        st.stats.prewarm_queued = st.prewarm.len();
                        st.running += 1;
                        break j;
                    }
                    st = self.work.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            match job {
                Job::Stage(h) => drop(self.library(h)),
                Job::Program(h) => self.run_program(h),
                Job::Pipeline(k) => self.run_pipeline(k),
                Job::Persist => self.run_persist(),
            }
            self.lock().running -= 1;
            self.done.notify_all();
        }
    }
}

impl LibError {
    fn label(self, entry: &str) -> LibError {
        match self {
            LibError::Failed(e) => LibError::Failed(format!("Metal compile of {entry}: {e}")),
            m => m,
        }
    }

    fn message(self) -> String {
        match self {
            LibError::Failed(e) => e,
            LibError::Missing => "stage source missing".into(),
        }
    }
}

impl<B: PipelineBackend> Compiler<B> {
    /// Starts `workers` threads (each runs `on_thread` first: thread QoS) and queues last launches' manifest in
    /// `dir` (None: nothing persisted) for background compilation.
    pub fn new(backend: B, dir: Option<PathBuf>, workers: usize, on_thread: fn()) -> Self {
        let manifest = dir.as_deref().map(Manifest::load).unwrap_or_default();
        let mut st = State {
            demand: VecDeque::new(),
            prewarm: VecDeque::new(),
            sources: HashMap::new(),
            libs: HashMap::new(),
            infos: HashMap::new(),
            programs: HashMap::new(),
            pipes: HashMap::new(),
            events: Vec::new(),
            manifest,
            running: 0,
            persisting: false,
            shutdown: false,
            stats: Stats::default(),
        };
        // programs first (their libraries, in parallel), then the pipelines, grouped by program
        let mut infos: Vec<ProgramInfo> = st.manifest.programs().cloned().collect();
        infos.sort_by_key(|p| p.hash());
        for info in infos {
            let h = info.hash();
            st.infos.insert(h, Arc::new(info));
            st.programs.insert(h, Entry { work: Work::Queued, wanted: false });
            st.prewarm.push_back(Job::Program(h));
        }
        let mut keys: Vec<PipelineKey> = st.manifest.pipelines().copied().collect();
        keys.sort_by_key(|k| (k.program, k.sig));
        for k in keys {
            st.pipes.insert(k, Entry { work: Work::Queued, wanted: false });
            st.prewarm.push_back(Job::Pipeline(k));
        }
        (st.stats.manifest_programs, st.stats.manifest_pipelines) = st.manifest.len();
        st.stats.prewarm_queued = st.prewarm.len();
        let shared = Arc::new(Shared { backend, dir, started: std::time::SystemTime::now(), state: Mutex::new(st), work: Condvar::new(), done: Condvar::new() });
        for i in 0..workers.max(1) {
            let s = shared.clone();
            let spawned = std::thread::Builder::new().name(format!("fr-compile-{i}")).spawn(move || {
                on_thread();
                s.worker()
            });
            if let Err(e) = spawned {
                eprintln!("pipeline compiler: worker thread not started: {e}");
            }
        }
        Compiler { shared }
    }

    pub fn backend(&self) -> &B { &self.shared.backend }

    /// A program the game uses: its two libraries compile in the background ([`Event::Program`] follows; at once
    /// when the manifest already compiled it). Returns the program's hash.
    pub fn add_program(&self, info: ProgramInfo, vertex_msl: Arc<str>, fragment_msl: Arc<str>) -> Hash {
        let h = info.hash();
        let mut st = self.shared.lock();
        let fragment = info.fragment;
        for (stage, msl) in [(info.vertex, vertex_msl), (fragment, fragment_msl)] {
            // an identical stage came with an earlier program (or the manifest compiled it): one library for both
            if st.libs.contains_key(&stage) || st.sources.contains_key(&stage) {
                st.stats.libraries_shared += 1;
            }
            // kept until the library exists: a manifest job that is looking for this stage on disk may not find it
            if !matches!(st.libs.get(&stage), Some(Lib::Ready(_) | Lib::Failed(_))) {
                st.sources.entry(stage).or_insert(msl);
            }
        }
        st.manifest.use_program(&info);
        st.infos.entry(h).or_insert_with(|| Arc::new(info));
        let e = st.programs.entry(h).or_insert(Entry { work: Work::Queued, wanted: false });
        let first = !std::mem::replace(&mut e.wanted, true);
        let event = match &e.work {
            Work::Done(r) => Some(r.clone()),
            _ => None,
        };
        let queue = first && matches!(e.work, Work::Queued);
        match event {
            Some(result) => st.events.push(Event::Program { hash: h, result }),
            None if first => st.stats.pending_programs += 1,
            None => {}
        }
        if queue {
            // ahead of the manifest's queue (a second copy there is skipped when its turn comes); the fragment
            // stage as its own job: another worker compiles it while this program's job is on the vertex stage
            st.demand.push_back(Job::Program(h));
            st.demand.push_back(Job::Stage(fragment));
            self.shared.work.notify_all();
        }
        h
    }

    /// The pipeline for `key` (its program was added). Pending: it is queued ahead of the prewarm work and an
    /// [`Event::Pipeline`] follows.
    pub fn request(&self, key: &PipelineKey) -> Status<B::Pipeline> {
        let mut st = self.shared.lock();
        st.manifest.use_pipeline(key);
        let e = st.pipes.entry(*key).or_insert(Entry { work: Work::Queued, wanted: false });
        if let Work::Done(r) = &e.work {
            return match r {
                Ok(p) => Status::Ready(p.clone()),
                Err(e) => Status::Failed(e.clone()),
            };
        }
        let first = !std::mem::replace(&mut e.wanted, true);
        let queue = first && matches!(e.work, Work::Queued);
        if first {
            st.stats.pending_pipelines += 1;
        }
        if queue {
            st.demand.push_back(Job::Pipeline(*key));
            self.shared.work.notify_one();
        }
        Status::Pending
    }

    /// What finished since the last call.
    pub fn poll(&self) -> Vec<Event<B::Pipeline>> { std::mem::take(&mut self.shared.lock().events) }

    pub fn stats(&self) -> Stats { self.shared.lock().stats }

    /// Blocks until the requested pipeline is built or has failed (its event is then waiting in `poll`).
    pub fn wait_pipeline(&self, key: &PipelineKey) {
        let mut st = self.shared.lock();
        while matches!(st.pipes.get(key), Some(Entry { work: Work::Queued | Work::Building, wanted: true })) {
            st = self.shared.wait(st);
        }
    }

    /// Blocks until an added program's libraries are compiled or have failed.
    pub fn wait_program(&self, h: Hash) {
        let mut st = self.shared.lock();
        while matches!(st.programs.get(&h), Some(Entry { work: Work::Queued | Work::Building, wanted: true })) {
            st = self.shared.wait(st);
        }
    }

    /// Blocks until everything added / requested is compiled and no persist is under way. The manifest's own
    /// queue (prewarm work nothing asked for yet) may still be running.
    pub fn wait_demand(&self) {
        let mut st = self.shared.lock();
        while st.stats.pending_programs + st.stats.pending_pipelines > 0 || st.persisting {
            st = self.shared.wait(st);
        }
    }

    /// Blocks until nothing is queued or compiling: everything added / requested and the whole manifest.
    pub fn wait_idle(&self) {
        let mut st = self.shared.lock();
        while !st.demand.is_empty() || !st.prewarm.is_empty() || st.running > 0 {
            st = self.shared.wait(st);
        }
    }

    /// True when the manifest has entries this launch added (or re-used) that are not on disk yet.
    pub fn dirty(&self) -> bool { self.shared.lock().manifest.dirty }

    /// Write the manifest and the backend's archive on a worker ([`Event::Persisted`] follows). No-op while one
    /// is under way.
    pub fn persist(&self) {
        let mut st = self.shared.lock();
        if !std::mem::replace(&mut st.persisting, true) {
            st.demand.push_back(Job::Persist);
            self.shared.work.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Fake {
        libs: AtomicUsize,
        pipes: AtomicUsize,
        persists: AtomicUsize,
        archive: Mutex<std::collections::HashSet<PipelineKey>>,
    }

    impl PipelineBackend for Arc<Fake> {
        type Library = Arc<str>;
        type Pipeline = String;
        fn library(&self, msl: &str) -> Result<Arc<str>, String> {
            self.libs.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            if msl.contains("bad") { Err("syntax".into()) } else { Ok(Arc::from(msl)) }
        }
        fn pipeline(&self, j: &PipelineJob<'_, Arc<str>>) -> Result<(String, bool), String> {
            self.pipes.fetch_add(1, Ordering::SeqCst);
            let hit = !self.archive.lock().unwrap().insert(*j.key);
            Ok((format!("{}+{}:{}:{:?}", j.vertex, j.fragment, j.key.sig, j.key.target), hit))
        }
        fn persist(&self) -> Result<Option<String>, String> {
            self.persists.fetch_add(1, Ordering::SeqCst);
            Ok(Some("archived".into()))
        }
    }

    fn info(v: &str, f: &str) -> ProgramInfo { ProgramInfo::new("p", v, f, "main0", "main0", vec![Attr { location: 0, comps: 3, cols: 1 }]) }

    fn key(p: &ProgramInfo, sig: u64, target: Target) -> PipelineKey { PipelineKey { program: p.hash(), blend: None, color_write: true, sig, target } }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fr-pipelines-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn settle<B: PipelineBackend>(c: &Compiler<B>) -> Vec<Event<B::Pipeline>> {
        c.wait_idle();
        c.poll()
    }

    #[test]
    fn hash_is_stable_and_separates_parts() {
        assert_eq!(hex(hash128(&[b"abc"])), hex(hash128(&[b"abc"])));
        assert_ne!(hash128(&[b"ab", b"c"]), hash128(&[b"a", b"bc"]));
        assert_ne!(hash128(&[b"abcdefgh"]), hash128(&[b"abcdefgi"]));
        assert_ne!(hash128(&[b""]), hash128(&[b"\0"]));
        // the on-disk names depend on it: a change here orphans every stored stage
        assert_eq!(hex(hash128(&[b"fastrender"])), "4be6de7128431f2e45047d4285b4c82c");
    }

    #[test]
    fn identical_stages_compile_once_and_pipelines_arrive_as_events() {
        let fake = Arc::new(Fake::default());
        let c = Compiler::new(fake.clone(), None, 4, || {});
        // three programs share one vertex stage; two of them share the fragment stage too (different entry)
        let (a, b) = (info("vs", "fs-a"), info("vs", "fs-b"));
        let mut a2 = info("vs", "fs-a");
        a2.fragment_entry = "other".into();
        let (ha, hb, ha2) = (
            c.add_program(a.clone(), "vs".into(), "fs-a".into()),
            c.add_program(b.clone(), "vs".into(), "fs-b".into()),
            c.add_program(a2.clone(), "vs".into(), "fs-a".into()),
        );
        assert!(ha != hb && ha != ha2);
        let k = key(&a, 3, Target::Main);
        assert!(matches!(c.request(&k), Status::Pending | Status::Ready(_)));
        c.wait_pipeline(&k);
        let ev = settle(&c);
        assert_eq!(fake.libs.load(Ordering::SeqCst), 3, "vs, fs-a, fs-b: once each");
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Program { result: Ok(()), .. })).count(), 3);
        assert!(ev.iter().any(|e| matches!(e, Event::Pipeline { key, result: Ok(p) } if *key == k && p == "vs+fs-a:3:Main")));
        // asked again: ready at once, nothing compiled
        assert!(matches!(c.request(&k), Status::Ready(_)));
        assert_eq!(fake.pipes.load(Ordering::SeqCst), 1);
        let s = c.stats();
        assert_eq!((s.libraries, s.pipelines, s.pending_programs, s.pending_pipelines), (3, 1, 0, 0));
        assert_eq!(s.libraries_shared, 3, "b's vertex stage, a2's two stages");
    }

    #[test]
    fn a_failed_stage_fails_its_program_and_pipelines_once() {
        let fake = Arc::new(Fake::default());
        let c = Compiler::new(fake.clone(), None, 2, || {});
        let p = info("vs", "bad fs");
        let h = c.add_program(p.clone(), "vs".into(), "bad fs".into());
        c.wait_program(h);
        let k = key(&p, 3, Target::Depth);
        let _ = c.request(&k);
        c.wait_pipeline(&k);
        let ev = settle(&c);
        assert!(ev.iter().any(|e| matches!(e, Event::Program { result: Err(m), .. } if m.contains("Metal compile of main0: syntax"))));
        assert!(ev.iter().any(|e| matches!(e, Event::Pipeline { result: Err(_), .. })));
        assert!(matches!(c.request(&k), Status::Failed(_)));
        assert_eq!(fake.libs.load(Ordering::SeqCst), 2, "the bad stage is compiled once, not once per pipeline");
        assert_eq!(c.stats().pending_pipelines, 0);
    }

    #[test]
    fn the_manifest_prewarms_the_next_launch_from_stored_stages() {
        let dir = temp("prewarm");
        let (p, q) = (info("vs-1", "fs-1"), info("vs-1", "fs-2"));
        let keys = [key(&p, 3, Target::Main), key(&p, 3, Target::Depth), key(&q, 0x83, Target::Rt(7))];
        {
            let fake = Arc::new(Fake::default());
            let c = Compiler::new(fake.clone(), Some(dir.clone()), 3, || {});
            assert_eq!(c.stats().manifest_pipelines, 0);
            c.add_program(p.clone(), "vs-1".into(), "fs-1".into());
            c.add_program(q.clone(), "vs-1".into(), "fs-2".into());
            for k in &keys {
                let _ = c.request(k);
            }
            c.wait_idle();
            assert!(c.dirty());
            c.persist();
            let ev = settle(&c);
            assert!(ev.iter().any(|e| matches!(e, Event::Persisted(Ok(Some(_))))));
            assert!(!c.dirty());
            assert_eq!(fake.persists.load(Ordering::SeqCst), 1);
            c.wait_demand(); // nothing pending, no persist running: returns at once
        }
        // second launch: everything compiles before the game adds a program, from the stored MSL
        let fake = Arc::new(Fake::default());
        let c = Compiler::new(fake.clone(), Some(dir.clone()), 3, || {});
        assert_eq!((c.stats().manifest_programs, c.stats().manifest_pipelines), (2, 3));
        c.wait_idle();
        assert_eq!((fake.libs.load(Ordering::SeqCst), fake.pipes.load(Ordering::SeqCst)), (3, 3));
        assert_eq!((c.stats().prewarmed_programs, c.stats().prewarmed_pipelines), (2, 3));
        assert!(c.poll().is_empty(), "nothing was asked for yet: no events");
        // the game arrives: its program and pipelines are ready at once
        let h = c.add_program(p.clone(), "vs-1".into(), "fs-1".into());
        assert!(c.poll().iter().any(|e| matches!(e, Event::Program { hash, result: Ok(()) } if *hash == h)));
        assert!(matches!(c.request(&keys[0]), Status::Ready(s) if s == "vs-1+fs-1:3:Main"));
        assert!(matches!(c.request(&keys[1]), Status::Ready(_)));
        assert_eq!((fake.libs.load(Ordering::SeqCst), fake.pipes.load(Ordering::SeqCst)), (3, 3));
        assert_eq!(c.stats().pending_programs + c.stats().pending_pipelines, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_demanded_pipeline_overtakes_the_prewarm_queue_and_missing_stages_are_not_failures() {
        let dir = temp("missing");
        let p = info("vs-m", "fs-m");
        let k = key(&p, 3, Target::Main);
        {
            let c = Compiler::new(Arc::new(Fake::default()), Some(dir.clone()), 1, || {});
            c.add_program(p.clone(), "vs-m".into(), "fs-m".into());
            let _ = c.request(&k);
            c.wait_idle();
            c.persist();
            c.wait_idle();
        }
        // the stage sources are gone (cache cleared by hand), the manifest is not
        std::fs::remove_dir_all(dir.join("stages")).unwrap();
        let fake = Arc::new(Fake::default());
        let c = Compiler::new(fake.clone(), Some(dir.clone()), 1, || {});
        c.wait_idle();
        assert_eq!((fake.libs.load(Ordering::SeqCst), c.stats().prewarmed_pipelines), (0, 0));
        // the program arrives with its sources: compiled as if there were no manifest
        let h = c.add_program(p.clone(), "vs-m".into(), "fs-m".into());
        assert!(matches!(c.request(&k), Status::Pending));
        c.wait_program(h);
        c.wait_pipeline(&k);
        let ev = c.poll();
        assert!(ev.iter().any(|e| matches!(e, Event::Pipeline { result: Ok(_), .. })));
        assert!(ev.iter().any(|e| matches!(e, Event::Program { result: Ok(()), .. })));
        // a stage file that is not the source its name says is not compiled either
        std::fs::write(stage_path(&dir, p.vertex), "tampered").unwrap();
        assert!(read_stage(&dir, p.vertex).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_age_out_after_keep_runs_unused_launches() {
        let dir = temp("age");
        let (p, q) = (info("vs-a", "fs-a"), info("vs-b", "fs-b"));
        let mut m = Manifest::load(&dir);
        m.use_program(&p);
        m.use_program(&q);
        m.use_pipeline(&key(&p, 1, Target::Main));
        m.use_pipeline(&key(&q, 1, Target::Main));
        m.save(&dir).unwrap();
        for _ in 0..KEEP_RUNS {
            let mut m = Manifest::load(&dir);
            assert_eq!(m.len(), (2, 2));
            m.use_program(&p); // only p is still played
            m.use_pipeline(&key(&p, 1, Target::Main));
            m.save(&dir).unwrap();
        }
        let m = Manifest::load(&dir);
        assert_eq!(m.len(), (1, 1));
        assert_eq!(m.programs().next().unwrap().hash(), p.hash());
        // another version's manifest is ignored
        std::fs::write(dir.join("manifest.json"), r#"{"version":0,"run":3,"programs":[],"pipelines":[]}"#).unwrap();
        assert_eq!(Manifest::load(&dir).len(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_round_trips_every_key_shape() {
        let dir = temp("roundtrip");
        let p = info("v", "f");
        let keys = [
            PipelineKey { program: p.hash(), blend: Some([204, 205, 201, 205, 100, 100]), color_write: false, sig: u64::MAX, target: Target::Rt(0xFFFF_FFFF) },
            key(&p, 0, Target::Rg16),
            key(&p, 0x8843, Target::Depth),
        ];
        let mut m = Manifest::load(&dir);
        m.use_program(&p);
        keys.iter().for_each(|k| m.use_pipeline(k));
        m.save(&dir).unwrap();
        let back = Manifest::load(&dir);
        let mut got: Vec<PipelineKey> = back.pipelines().copied().collect();
        got.sort_by_key(|k| k.sig);
        let mut want = keys.to_vec();
        want.sort_by_key(|k| k.sig);
        assert_eq!(got, want);
        assert_eq!(back.programs().next().unwrap(), &p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
