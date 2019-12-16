//! This crate provides a client to the watchman file watching service.
//!
//! Start with the [Connector](struct.Connector.html) struct and use
//! it to connect and return a [Client](struct.Client.html) struct,
//! [Client::resolve_root](struct.Client.html#method.resolve_root) to
//! resolve a path and initiate a watch, and then
//! [Client::query](struct.Client.html#method.query) to perform
//! a query, or [Client::subscribe](struct.Client.html#method.subscribe)
//! to subscribe to file changes in real time.
//!
//! This example shows how to connect and expand a glob from the
//! current working directory:
//!
//! ```norun
//! use watchman_client::prelude::*;
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!   let mut client = Connector::new().connect().await?;
//!   let resolved = client
//!      .resolve_root(CanonicalPath::canonicalize(".")?)
//!      .await?;
//!
//!   // Basic globs -> names
//!   let files = client.glob(&resolved, &["**/*.rs"]).await?;
//!   println!("files: {:#?}", files);
//!   Ok(())
//! }
//! ```
pub mod expr;
pub mod fields;
mod named_pipe;
pub mod pdu;
use serde_bser::de::{Bunser, PduInfo, SliceRead};
use serde_bser::value::Value;
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::prelude::*;
use tokio::process::Command;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

/// The next id number to use when generating a subscription name
static SUB_ID: AtomicUsize = AtomicUsize::new(1);

/// `use watchman_client::prelude::*` for convenient access to the types
/// provided by this crate
pub mod prelude {
    pub use crate::expr::*;
    pub use crate::fields::*;
    pub use crate::pdu::*;
    pub use crate::query_result_type;
    pub use crate::{CanonicalPath, Client, Connector, ResolvedRoot};
}

use prelude::*;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO Error: {0}")]
    Tokio(#[from] tokio::io::Error),
    #[error("While invoking the {watchman_path} CLI to discover the server connection details: {reason}, stderr=`{stderr}`")]
    ConnectionDiscovery {
        watchman_path: PathBuf,
        reason: String,
        stderr: String,
    },
    #[error("The watchman server reported an error: \"{}\", while executing command: {}", .message, .command)]
    WatchmanServerError { message: String, command: String },
    #[error("The watchman server reported an error: \"{}\"", .message)]
    WatchmanResponseError { message: String },
    #[error("The watchman server didn't return a value for field `{}` in response to a `{}` command. {:?}", .fieldname, .command, .response)]
    MissingField {
        fieldname: &'static str,
        command: String,
        response: String,
    },
    #[error("Unexpected EOF from server")]
    Eof,

    #[error("{source} (data: {data:x?})")]
    Deserialize {
        source: Box<dyn std::error::Error + Send>,
        data: Vec<u8>,
    },

    #[error("{source}")]
    Serialize {
        source: Box<dyn std::error::Error + Send>,
    },

    #[error("while attempting to connect to {endpoint}: {source}")]
    Connect {
        endpoint: PathBuf,
        source: Box<dyn std::error::Error + Send>,
    },

    #[error("{0}")]
    Generic(String),
}

impl Error {
    fn generic<T: std::fmt::Display>(error: T) -> Self {
        Self::Generic(format!("{}", error))
    }
}

/// The Connector defines how to connect to the watchman server.
/// You will typically use `Connector::new` to set up the connection with
/// the environmental defaults.  You might want to override those defaults
/// in situations such as integration testing environments, or in extremely
/// latency sensitive environments where the cost of performing discovery
/// is a measurable overhead.
#[derive(Default)]
pub struct Connector {
    watchman_cli_path: Option<PathBuf>,
    unix_domain: Option<PathBuf>,
}

impl Connector {
    /// Set up the connector with the system defaults.
    /// If `WATCHMAN_SOCK` is set in the environment it will preset the
    /// local IPC socket path.
    /// Otherwise the connector will invoke the watchman CLI to perform
    /// discovery.
    pub fn new() -> Self {
        let connector = Self::default();

        if let Some(val) = std::env::var_os("WATCHMAN_SOCK") {
            connector.unix_domain_socket(val)
        } else {
            connector
        }
    }

    /// If the watchman CLI is installed in a location that is not present
    /// in the PATH environment variable, this method is used to inform
    /// the connector of its location.
    pub fn watchman_cli_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.watchman_cli_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Specify the unix domain socket path
    pub fn unix_domain_socket<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.unix_domain = Some(path.as_ref().to_path_buf());
        self
    }

    /// Resolve the unix domain socket path, taking either the override
    /// or performing discovery.
    async fn resolve_unix_domain_path(&self) -> Result<PathBuf, Error> {
        if let Some(path) = self.unix_domain.as_ref() {
            Ok(path.clone())
        } else {
            let watchman_path = self
                .watchman_cli_path
                .as_ref()
                .map(|p| p.as_ref())
                .unwrap_or_else(|| Path::new("watchman"));

            let output = Command::new(watchman_path)
                .args(&["--output-encoding", "bser-v2", "get-sockname"])
                .output()
                .await
                .map_err(|source| Error::ConnectionDiscovery {
                    watchman_path: watchman_path.to_path_buf(),
                    reason: source.to_string(),
                    stderr: "".to_string(),
                })?;

            let info: GetSockNameResponse =
                serde_bser::from_slice(&output.stdout).map_err(|source| {
                    Error::ConnectionDiscovery {
                        watchman_path: watchman_path.to_path_buf(),
                        reason: source.to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    }
                })?;

            let debug = format!("{:#?}", info);

            if let Some(message) = info.error {
                return Err(Error::WatchmanServerError {
                    message,
                    command: "get-sockname".into(),
                });
            }

            info.sockname.ok_or_else(|| Error::MissingField {
                fieldname: "sockname",
                command: "get-sockname".into(),
                response: debug,
            })
        }
    }

    /// Establish a connection to the watchman server.
    /// If the connector was configured to perform discovery (which is
    /// the default configuration), then this will attempt to start
    /// the watchman server.
    pub async fn connect(self) -> Result<Client, Error> {
        let sock_path = self.resolve_unix_domain_path().await?;

        #[cfg(unix)]
        let stream: Box<dyn ReadWriteStream> = Box::new(UnixStream::connect(sock_path).await?);

        #[cfg(windows)]
        let stream: Box<dyn ReadWriteStream> =
            Box::new(named_pipe::NamedPipe::connect(sock_path).await?);

        let (reader, writer) = tokio::io::split(stream);

        let (request_tx, request_rx) = tokio::sync::mpsc::channel(128);

        let mut reader_task = ReaderTask {
            reader,
            request_tx: request_tx.clone(),
        };
        tokio::spawn(async move {
            if let Err(err) = reader_task.run().await {
                eprintln!("watchman reader task failed: {}", err);
            }
        });

        let mut task = ClientTask {
            writer,
            request_rx,
            request_queue: VecDeque::new(),
            waiting_response: false,
            subscriptions: HashMap::new(),
        };
        tokio::spawn(async move {
            if let Err(err) = task.run().await {
                eprintln!("watchman client task failed: {}", err);
            }
        });

        let inner = Arc::new(Mutex::new(ClientInner { request_tx }));

        Ok(Client { inner })
    }
}

/// Represents a canonical path in the filesystem.
#[derive(Debug)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    /// Construct the canonical version of the supplied path.
    /// This function will canonicalize the path and return the
    /// result, if successful.
    /// If you have already canonicalized the path, it is preferable
    /// to use the `with_canonicalized_path` function instead.
    pub fn canonicalize<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let path = std::fs::canonicalize(path)?;
        Ok(Self(Self::strip_unc_escape(path)))
    }

    /// Construct from an already canonicalized path.
    /// This function will panic if the supplied path is not an absolute
    /// path!
    pub fn with_canonicalized_path(path: PathBuf) -> Self {
        assert!(
            path.is_absolute(),
            "attempted to call \
             CanonicalPath::with_canonicalized_path on a non-canonical path! \
             You probably want to call CanonicalPath::canonicalize instead!"
        );
        Self(Self::strip_unc_escape(path))
    }

    /// Watchman doesn't like the UNC prefix being present for incoming paths
    /// in its current implementation: we should fix that, but in the meantime
    /// we want clients to be able to connect to existing versions, so let's
    /// strip off the UNC escape
    #[cfg(windows)]
    #[inline]
    fn strip_unc_escape(path: PathBuf) -> PathBuf {
        match path.to_str() {
            Some(s) if s.starts_with("\\\\?\\") => PathBuf::from(&s[4..]),
            _ => path,
        }
    }

    #[cfg(unix)]
    #[inline]
    fn strip_unc_escape(path: PathBuf) -> PathBuf {
        path
    }
}

/// Data that describes a watched filesystem location.
/// Watchman performs watch aggregation to project boundaries, so a request
/// to watch a subdirectory will resolve to the higher level root path
/// and a relative path offset.
/// This struct encodes both pieces of information.
#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    root: PathBuf,
    relative: Option<PathBuf>,
    watcher: String,
}

impl ResolvedRoot {
    /// Returns the name of the watcher that the server is using to
    /// monitor the path.  The watcher is generally system dependent,
    /// but some systems offer multipler watchers.
    /// You generally don't care too much about the watcher that is
    /// in use, but if the watcher is a virtualized filesystem such as
    /// `eden` then you may wish to use to alternative queries to get the
    /// best performance.
    pub fn watcher(&self) -> &str {
        self.watcher.as_str()
    }

    /// Returns the root of the watchman project that is being watched
    pub fn project_root(&self) -> &Path {
        &self.root
    }

    /// Returns the absolute path to the directory that you requested be resolved.
    pub fn path(&self) -> PathBuf {
        if let Some(relative) = self.relative.as_ref() {
            self.root.join(relative)
        } else {
            self.root.clone()
        }
    }

    /// Returns the path to the directory that you requested be resolved,
    /// relative to the `project_root`.
    pub fn project_relative_path(&self) -> Option<&Path> {
        self.relative.as_ref().map(PathBuf::as_ref)
    }
}

trait ReadWriteStream: AsyncRead + AsyncWrite + std::marker::Unpin + Send {}

#[cfg(unix)]
impl ReadWriteStream for UnixStream {}

struct SendRequest {
    /// The serialized request to send to the server
    buf: Vec<u8>,
    /// to pass the response back to the requstor
    tx: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
}

impl SendRequest {
    fn respond(self, result: Result<Vec<u8>, String>) -> Result<(), Error> {
        self.tx
            .send(result)
            .map_err(|_| Error::generic("requestor has dropped its receiver"))
    }
}

enum TaskItem {
    QueueRequest(SendRequest),
    ProcessReceivedPdu(Vec<u8>),
    RegisterSubscription(String, UnboundedSender<Vec<u8>>),
}

/// A live connection to a watchman server.
/// Use [Connector](struct.Connector.html) to establish a connection.
pub struct Client {
    inner: Arc<Mutex<ClientInner>>,
}

/// The reader task lives to read a PDU and send it to the ClientTask
struct ReaderTask {
    reader: tokio::io::ReadHalf<Box<dyn ReadWriteStream>>,
    request_tx: Sender<TaskItem>,
}

impl ReaderTask {
    async fn run(&mut self) -> Result<(), Error> {
        loop {
            let pdu = self.read_pdu_vec().await?;
            self.request_tx
                .send(TaskItem::ProcessReceivedPdu(pdu))
                .await
                .map_err(Error::generic)?;
        }
    }

    /// Sniffs out the BSER PDU header to determine the length of data that
    /// needs to be read in order to decode the full PDU
    async fn read_bser_pdu_length(&mut self) -> Result<PduHeader, Error> {
        // We know that the smallest full PDU returned by the server
        // won't ever be smaller than this size
        const BUF_SIZE: usize = 16;
        let mut buf = [0u8; BUF_SIZE];

        let pos = self.reader.read(&mut buf).await?;
        if pos == 0 {
            return Err(Error::Eof);
        }

        let buf = &buf[..pos];

        let mut bunser = Bunser::new(SliceRead::new(buf));
        let pdu = bunser.read_pdu().map_err(|source| Error::Deserialize {
            source: Box::new(source),
            data: buf.to_vec(),
        })?;
        let buf = buf.to_vec();
        Ok(PduHeader { buf, pdu })
    }

    /// Read the bytes that comprise a BSER encoded PDU
    async fn read_pdu_vec(&mut self) -> Result<Vec<u8>, Error> {
        let header = self.read_bser_pdu_length().await?;
        let total_size = (header.pdu.start + header.pdu.len) as usize;
        let mut buf = header.buf;

        let mut end = buf.len();

        buf.resize(total_size, 0);

        while end != total_size {
            let n = self
                .reader
                .read(&mut buf.as_mut_slice()[end..total_size])
                .await?;
            if n == 0 {
                return Err(Error::Eof);
            }
            end += n;
        }

        Ok(buf)
    }
}

/// The client task coordinates sending requests with processing
/// unilateral results
struct ClientTask {
    writer: tokio::io::WriteHalf<Box<dyn ReadWriteStream>>,
    request_rx: Receiver<TaskItem>,
    request_queue: VecDeque<SendRequest>,
    waiting_response: bool,
    subscriptions: HashMap<String, UnboundedSender<Vec<u8>>>,
}

impl Drop for ClientTask {
    fn drop(&mut self) {
        self.fail_all(&Error::generic("the client task terminated"));
    }
}

impl ClientTask {
    async fn run(&mut self) -> Result<(), Error> {
        // process things, and if we encounter an error, ensure that
        // we fail all outstanding requests
        match self.run_loop().await {
            Err(err) => {
                self.fail_all(&err);
                Err(err)
            }
            ok => ok,
        }
    }

    async fn run_loop(&mut self) -> Result<(), Error> {
        loop {
            match self.request_rx.recv().await {
                Some(TaskItem::QueueRequest(request)) => self.queue_request(request).await?,
                Some(TaskItem::ProcessReceivedPdu(pdu)) => self.process_pdu(pdu).await?,
                Some(TaskItem::RegisterSubscription(name, tx)) => {
                    self.register_subscription(name, tx)
                }
                None => break,
            };
        }
        Ok(())
    }

    fn register_subscription(&mut self, name: String, tx: UnboundedSender<Vec<u8>>) {
        self.subscriptions.insert(name, tx);
    }

    /// Generate an error for each queued request.
    /// This is called in situations where the state of the connection
    /// to the serve is non-recoverable.
    fn fail_all(&mut self, err: &Error) {
        while let Some(request) = self.request_queue.pop_front() {
            request.respond(Err(err.to_string())).ok();
        }
    }

    /// If we're not waiting for the response to a request,
    /// then send the next one!
    async fn send_next_request(&mut self) -> Result<(), Error> {
        if !self.waiting_response && !self.request_queue.is_empty() {
            match self
                .writer
                .write_all(&self.request_queue.front().expect("not empty").buf)
                .await
            {
                Err(err) => {
                    // A failed write breaks our world; we don't want to
                    // try to continue
                    return Err(err.into());
                }
                Ok(_) => self.waiting_response = true,
            }
        }
        Ok(())
    }

    /// Queue up a new request from the client code, and then
    /// check to see if we can send a queued request to the server.
    async fn queue_request(&mut self, request: SendRequest) -> Result<(), Error> {
        self.request_queue.push_back(request);
        self.send_next_request().await?;
        Ok(())
    }

    /// Dispatch a PDU that we just read to the appropriate client code.
    async fn process_pdu(&mut self, pdu: Vec<u8>) -> Result<(), Error> {
        use serde::Deserialize;
        #[derive(Deserialize, Debug)]
        pub struct Unilateral {
            pub unilateral: bool,
            pub subscription: String,
        }

        if let Ok(unilateral) = bunser::<Unilateral>(&pdu) {
            if let Some(subscription) = self.subscriptions.get_mut(&unilateral.subscription) {
                if let Err(_) = subscription.send(pdu) {
                    // The `Subscription` was dropped; we don't need to
                    // treat this as terminal for this client session,
                    // so just de-register the handler
                    self.subscriptions.remove(&unilateral.subscription);
                }
            }
        } else if self.waiting_response {
            let request = self
                .request_queue
                .pop_front()
                .expect("waiting_response is only true when request_queue is not empty");
            self.waiting_response = false;

            request.respond(Ok(pdu))?;
        } else {
            // This should never happen as we're not doing any subscription stuff
            return Err(Error::generic("received a unilateral PDU from the server"));
        }

        self.send_next_request().await?;
        Ok(())
    }
}

struct PduHeader {
    buf: Vec<u8>,
    pdu: PduInfo,
}

fn bunser<T>(buf: &[u8]) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    let response: T = serde_bser::from_slice(&buf).map_err(|source| Error::Deserialize {
        source: Box::new(source),
        data: buf.to_vec(),
    })?;
    Ok(response)
}

struct ClientInner {
    request_tx: Sender<TaskItem>,
}

impl ClientInner {
    /// This method will send a request to the watchman server
    /// and wait for its response.
    /// This is really an internal method, but it is made public in case a
    /// consumer of this crate needs to issue a command for which we haven't
    /// yet made an ergonomic wrapper.
    pub(crate) async fn generic_request<Request, Response>(
        &mut self,
        request: Request,
    ) -> Result<Response, Error>
    where
        Request: serde::Serialize + std::fmt::Debug,
        Response: serde::de::DeserializeOwned,
    {
        // Step 1: serialize into a bser byte buffer
        let mut request_data = vec![];
        serde_bser::ser::serialize(&mut request_data, &request).map_err(|source| {
            Error::Serialize {
                source: Box::new(source),
            }
        })?;

        // Step 2: ask the client task to send it for us
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.request_tx
            .send(TaskItem::QueueRequest(SendRequest {
                buf: request_data,
                tx,
            }))
            .await
            .map_err(Error::generic)?;

        // Step 3: wait for the client task to give us the response
        let pdu_data = rx.await.map_err(Error::generic)?.map_err(Error::generic)?;

        // Step 4: sniff for an error response in the deserialized data
        use serde::Deserialize;
        #[derive(Deserialize, Debug)]
        struct MaybeError {
            #[serde(default)]
            error: Option<String>,
        }

        // Step 5: deserialize into the caller-desired format
        let maybe_err: MaybeError = bunser(&pdu_data)?;
        if let Some(message) = maybe_err.error {
            return Err(Error::WatchmanServerError {
                message,
                command: format!("{:#?}", request),
            });
        }

        let response: Response = bunser(&pdu_data)?;
        Ok(response)
    }
}

/// Returned by [Subscription::next](struct.Subscription.html#method.next)
/// as events are observed by Watchman.
#[derive(Debug, Clone)]
pub enum SubscriptionData<F>
where
    F: serde::de::DeserializeOwned + std::fmt::Debug + Clone + QueryFieldList,
{
    /// The Subscription was canceled.
    /// This could be for a number of reasons that are not knowable
    /// to the client:
    /// * The user may have issued the `watch-del` command
    /// * The containing watch root may have been deleted or
    ///   un-mounted
    /// * The containing watch may no longer be accessible
    ///   to the watchman user/process
    /// * Some other error condition that renders the project
    ///   unwatchable may have occurred
    /// * The server may have been gracefully shutdown
    ///
    /// A Canceled subscription will deliver no further results.
    Canceled,

    /// Files matching your criteria have changed.
    /// The QueryResult contains the details.
    /// Pay attention to the
    /// [is_fresh_instance](pdu/struct.QueryResult.html#structfield.is_fresh_instance) field!
    FilesChanged(QueryResult<F>),

    /// Some other watchman client has broadcast that the watched
    /// project is entering a new named state.
    /// For example, `hg.update` may be generated by the FB
    /// internal source control system to indicate that the
    /// working copy is about to be updated to a new revision.
    /// The metadata field contains data specific to the named
    /// state.
    StateEnter {
        state_name: String,
        metadata: Option<Value>,
    },
    /// Some other watchman client has broadcast that the watched
    /// project is no longer in the named state.
    /// This event can also be generated if the watchman client
    /// that entered the state disconnects unexpectedly from
    /// the watchman server.
    /// The `metadata` field will be `None` in that situation.
    StateLeave {
        state_name: String,
        metadata: Option<Value>,
    },
}

/// A handle to a subscription initiated via `Client::subscribe`.
/// Repeatedly call `Subscription::next().await` to yield the next
/// set of subscription results.
/// Use the `cancel` method to gracefully halt this subscription
/// if you have a program that creates and destroys subscriptions
/// throughout its lifetime.
pub struct Subscription<F>
where
    F: serde::de::DeserializeOwned + std::fmt::Debug + Clone + QueryFieldList,
{
    name: String,
    inner: Arc<Mutex<ClientInner>>,
    root: ResolvedRoot,
    responses: UnboundedReceiver<Vec<u8>>,
    _phantom: PhantomData<F>,
}

impl<F> Subscription<F>
where
    F: serde::de::DeserializeOwned + std::fmt::Debug + Clone + QueryFieldList,
{
    /// Returns the assigned name for this subscription instance.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Yield the next set of subscription data.
    /// An error is generated if the subscription is disconnected
    /// from the server.
    pub async fn next(&mut self) -> Result<SubscriptionData<F>, Error> {
        let pdu = self
            .responses
            .recv()
            .await
            .ok_or_else(|| Error::generic("client was torn down"))?;

        let response: QueryResult<F> = bunser(&pdu)?;

        if response.subscription_canceled {
            self.responses.close();
            Ok(SubscriptionData::Canceled)
        } else if let Some(state_name) = response.state_enter {
            Ok(SubscriptionData::StateEnter {
                state_name,
                metadata: response.state_metadata,
            })
        } else if let Some(state_name) = response.state_leave {
            Ok(SubscriptionData::StateLeave {
                state_name,
                metadata: response.state_metadata,
            })
        } else {
            Ok(SubscriptionData::FilesChanged(response))
        }
    }

    /// Gracefully cancel this subscription.
    /// If you are imminently about to drop the associated client then you
    /// need not call this method.
    /// However, if the associated client is going to live much longer
    /// than a Subscription that you are about to drop,
    /// then it is recommended that you call `cancel` so that the server
    /// will stop delivering data about it.
    pub async fn cancel(self) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;
        let _: UnsubscribeResponse = inner
            .generic_request(Unsubscribe("unsubscribe", self.root.root, self.name))
            .await?;
        Ok(())
    }
}

impl Client {
    /// This method will send a request to the watchman server
    /// and wait for its response.
    /// This is really an internal method, but it is made public in case a
    /// consumer of this crate needs to issue a command for which we haven't
    /// yet made an ergonomic wrapper.
    #[doc(hidden)]
    pub async fn generic_request<Request, Response>(
        &self,
        request: Request,
    ) -> Result<Response, Error>
    where
        Request: serde::Serialize + std::fmt::Debug,
        Response: serde::de::DeserializeOwned,
    {
        let mut inner = self.inner.lock().await;
        let response: Response = inner.generic_request(request).await?;
        Ok(response)
    }

    /// This is typically the first method invoked on a client.
    /// Its purpose is to ensure that the watchman server is watching the specified
    /// path and to resolve it to a `ResolvedRoot` instance.
    ///
    /// The path to resolve must be a canonical path; watchman performs strict name
    /// resolution to detect TOCTOU issues and will generate an error if the path
    /// is not the canonical name.
    ///
    /// Note that for regular filesystem watches, if the requested path is not
    /// yet being watched, this method will not yield until the watchman server
    /// has completed a recursive crawl of that portion of the filesystem.
    /// In other words, the worst case performance of this is
    /// `O(recursive-number-of-files)` and is impacted by the underlying storage
    /// device and its performance characteristics.
    pub async fn resolve_root(&self, path: CanonicalPath) -> Result<ResolvedRoot, Error> {
        let response: WatchProjectResponse = self
            .generic_request(WatchProjectRequest("watch-project", path.0.clone()))
            .await?;

        Ok(ResolvedRoot {
            root: response.watch,
            relative: response.relative_path,
            watcher: response.watcher,
        })
    }

    /// Perform a generic watchman query.
    /// The `F` type is a struct defined by the
    /// [query_result_type!](macro.query_result_type.html) macro,
    /// or, if you want only the file name from the results, the
    /// [NameOnly](struct.NameOnly.html) struct.
    ///
    /// ```
    /// use watchman_client::prelude::*;
    /// use serde::Deserialize;
    ///
    /// query_result_type! {
    ///     struct NameAndType {
    ///         name: NameField,
    ///         file_type: FileTypeField,
    ///     }
    /// }
    ///
    /// async fn query(
    ///    client: &mut Client,
    ///    resolved: &ResolvedRoot
    /// ) -> Result<(), Box<dyn std::error::Error>> {
    ///    let response: QueryResult<NameAndType> = client
    ///        .query(
    ///            &resolved,
    ///               QueryRequestCommon {
    ///                glob: Some(vec!["**/*.rs".to_string()]),
    ///                ..Default::default()
    ///            },
    ///        )
    ///        .await?;
    ///    println!("response: {:#?}", response);
    ///    Ok(())
    /// }
    /// ```
    ///
    /// When constructing your result type, you can select from the
    /// following fields:
    ///
    /// * [CTimeAsFloatField](struct.CTimeAsFloatField.html)
    /// * [CTimeField](struct.CTimeField.html)
    /// * [ContentSha1HexField](struct.ContentSha1HexField.html)
    /// * [CreatedClockField](struct.CreatedClockField.html)
    /// * [DeviceNumberField](struct.DeviceNumberField.html)
    /// * [ExistsField](struct.ExistsField.html)
    /// * [FileTypeField](struct.FileTypeField.html)
    /// * [InodeNumberField](struct.InodeNumberField.html)
    /// * [MTimeAsFloatField](struct.MTimeAsFloatField.html)
    /// * [MTimeField](struct.MTimeField.html)
    /// * [ModeAndPermissionsField](struct.ModeAndPermissionsField.html)
    /// * [NameField](struct.NameField.html)
    /// * [NewField](struct.NewField.html)
    /// * [NumberOfLinksField](struct.NumberOfLinksField.html)
    /// * [ObservedClockField](struct.ObservedClockField.html)
    /// * [OwnerGidField](struct.OwnerGidField.html)
    /// * [OwnerUidField](struct.OwnerUidField.html)
    /// * [SizeField](struct.SizeField.html)
    /// * [SymlinkTargetField](struct.SymlinkTargetField.html)
    ///
    /// (See [the fields module](fields/index.html) for a definitive list)
    ///
    /// The file names are all relative to the `root` parameter.
    pub async fn query<F>(
        &self,
        root: &ResolvedRoot,
        query: QueryRequestCommon,
    ) -> Result<QueryResult<F>, Error>
    where
        F: serde::de::DeserializeOwned + std::fmt::Debug + Clone + QueryFieldList,
    {
        let query = QueryRequest(
            "query",
            root.root.clone(),
            QueryRequestCommon {
                relative_root: root.relative.clone(),
                fields: F::field_list(),
                ..query
            },
        );

        let response: QueryResult<F> = self.generic_request(query.clone()).await?;

        Ok(response)
    }

    /// Create a Subscription that will yield file changes as they occur in
    /// real time.
    /// The `F` type is a struct defined by the
    /// [query_result_type!](macro.query_result_type.html) macro,
    /// or, if you want only the file name from the results, the
    /// [NameOnly](struct.NameOnly.html) struct.
    ///
    /// Returns two pieces of information:
    /// * A [Subscription](struct.Subscription.html) handle that can be used to yield changes
    ///   as they are observed by watchman
    /// * A [SubscribeResponse](pdu/struct.SubscribeResponse.html) that contains some data about the
    ///   state of the watch at the time the subscription was
    ///   initiated
    pub async fn subscribe<F>(
        &self,
        root: &ResolvedRoot,
        query: SubscribeRequest,
    ) -> Result<(Subscription<F>, SubscribeResponse), Error>
    where
        F: serde::de::DeserializeOwned + std::fmt::Debug + Clone + QueryFieldList,
    {
        let name = format!(
            "sub-[{}]-{}",
            std::env::args()
                .nth(0)
                .unwrap_or_else(|| "<no-argv-0>".to_string()),
            SUB_ID.fetch_add(1, Ordering::Relaxed)
        );

        let query = SubscribeCommand(
            "subscribe",
            root.root.clone(),
            name.clone(),
            SubscribeRequest {
                relative_root: root.relative.clone(),
                fields: F::field_list(),
                ..query
            },
        );

        let (tx, responses) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut inner = self.inner.lock().await;
            inner
                .request_tx
                .send(TaskItem::RegisterSubscription(name.clone(), tx))
                .await
                .map_err(Error::generic)?;
        }

        let subscription = Subscription::<F> {
            name,
            inner: Arc::clone(&self.inner),
            root: root.clone(),
            responses,
            _phantom: PhantomData,
        };

        let response: SubscribeResponse = self.generic_request(query).await?;

        Ok((subscription, response))
    }

    /// Expand a set of globs into the set of matching file names.
    /// The globs must be relative to the `root` parameter.
    /// The returned file names are all relative to the `root` parameter.
    pub async fn glob(&self, root: &ResolvedRoot, globs: &[&str]) -> Result<Vec<PathBuf>, Error> {
        let response: QueryResult<NameOnly> = self
            .query(
                root,
                QueryRequestCommon {
                    relative_root: root.relative.clone(),
                    glob: Some(globs.iter().map(|&s| s.to_string()).collect()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(response
            .files
            .unwrap_or_else(Vec::new)
            .into_iter()
            .map(|f| f.name.into_inner())
            .collect())
    }

    /// Returns the current clock value for a watched root.
    /// If `sync_timeout` is `SyncTimeout::DisableCookie` then the instantaneous
    /// clock value is returned without using a sync cookie.
    ///
    /// Otherwise, a sync cookie will be created and the server will wait
    /// for up to the associated `sync_timeout` duration to observe it.
    /// If that timeout is reached, this method will yield an error.
    ///
    /// When should you use a cookie?  If you need to a clock value that is
    /// guaranteed to reflect any filesystem changes that happened before
    /// a given point in time you should use a sync cookie.
    ///
    /// ## See also:
    ///  * <https://facebook.github.io/watchman/docs/cmd/clock.html>
    ///  * <https://facebook.github.io/watchman/docs/cookies.html>
    pub async fn clock(
        &self,
        root: &ResolvedRoot,
        sync_timeout: SyncTimeout,
    ) -> Result<ClockSpec, Error> {
        let response: ClockResponse = self
            .generic_request(ClockRequest(
                "clock",
                root.root.clone(),
                ClockRequestParams { sync_timeout },
            ))
            .await?;
        Ok(response.clock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_builder_paths() {
        let builder = Connector::new().unix_domain_socket("/some/path");
        assert_eq!(builder.unix_domain, Some(PathBuf::from("/some/path")));
    }
}
