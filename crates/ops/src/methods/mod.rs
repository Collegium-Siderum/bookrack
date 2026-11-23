// SPDX-License-Identifier: Apache-2.0

//! One table of the library's methods, read by both transports.
//!
//! A [`MethodSpec`] declares everything a transport needs to serve a
//! method without a table of its own: the wire names it answers to on
//! the control plane and over MCP, what it touches ([`Access`]), how a
//! library selection reaches it ([`Selection`]), the JSON Schema of its
//! parameters on each surface, and the shape of its answer
//! ([`Output`]). [`LIBRARY_METHODS`] lists the methods that act on one
//! library; the host's own methods — the daemon, the queue, the
//! session — are listed by the host.
//!
//! [`dispatch::dispatch`] runs a method from the table. The body of
//! every method is written once there, and both transports reach it:
//! what differs between them is the envelope, the error codes, and the
//! caller the audit records, never the work.

pub mod dispatch;
pub mod params;
mod table;

pub use table::LIBRARY_METHODS;

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::{Map, Value};

/// A params type that carries a library selection.
///
/// The impl is what makes a [`Selection::Routed`] row checkable: the
/// table names the params type, the impl names the key and reads the
/// field behind it, so a method filed as routed whose params have no
/// such field does not compile.
pub trait RoutedParams {
    /// The params key that names the library.
    const LIBRARY_KEY: &'static str;

    /// The selection this call carries, `None` when the caller left it
    /// out and the registry routes the call.
    fn library(&self) -> Option<&str>;
}

/// What a write may lock, as the table declares it. Each kind names a
/// [`crate::write_session::Region`] without the item it addresses; the
/// dispatcher fills that in from the call's params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// The whole library.
    Library,
    /// Bringing new items in.
    Intake,
    /// One item's derived data.
    Item,
    /// One item's curated record.
    Curation,
    /// The entries of one reference book.
    Reference,
    /// The set of served libraries.
    Host,
}

/// What a method touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads the library's stores. A search still appends its own
    /// retrieval record.
    Read,
    /// Writes through a write session over `region`.
    Write {
        /// What the session locks.
        region: RegionKind,
    },
    /// Writes only the library's bookkeeping, through the bookkeeping
    /// door.
    Bookkeeping,
    /// Holds the connection open and streams notifications.
    Stream,
}

/// How an explicit library selection reaches a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// The method acts on one library, named under `control_key` on
    /// the control plane and under `mcp_key` over MCP.
    Routed {
        /// The control plane's params key for the library.
        control_key: &'static str,
        /// MCP's params key for the library.
        mcp_key: &'static str,
    },
    /// The method describes the process, not a library; a selection is
    /// meaningless but harmless.
    Process,
    /// The method answers about every library at once, so a selection
    /// cannot be honoured and is refused.
    Unrouted,
}

/// A function returning one JSON Schema object.
pub type SchemaFn = fn() -> Map<String, Value>;

/// The shape of a method's answer, as MCP publishes it.
#[derive(Debug, Clone, Copy)]
pub enum Output {
    /// Always a JSON object: the tool publishes this output schema and
    /// carries the answer as structured content beside its text.
    Structured(SchemaFn),
    /// A JSON object, or `null` when the addressed item does not
    /// exist: no output schema is published, and the answer is
    /// structured content only when it is an object.
    NullableObject,
    /// An array, an array or `null`, or an answer MCP does not carry:
    /// the text content alone.
    TextOnly,
}

/// One method of the table.
#[derive(Debug, Clone, Copy)]
pub struct MethodSpec {
    /// The method's name on the control plane.
    pub name: &'static str,
    /// The tool's name over MCP, or `None` when MCP does not carry the
    /// method.
    pub mcp_name: Option<&'static str>,
    /// What the method does, as an MCP client reads it in the tool
    /// list.
    pub summary: &'static str,
    /// What the method touches.
    pub access: Access,
    /// How a library selection reaches it.
    pub selection: Selection,
    /// Whether a host without a queue worker refuses it.
    pub needs_queue: bool,
    /// The schema of its params on the control plane.
    pub input_schema: SchemaFn,
    /// The schema of its params over MCP.
    pub mcp_input_schema: SchemaFn,
    /// The shape of its answer.
    pub output: Output,
}

/// Which transport a call arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The daemon's JSON-RPC control socket.
    ControlPlane,
    /// The MCP endpoint.
    Mcp,
}

impl MethodSpec {
    /// The name the method answers to on `surface`. A method MCP does
    /// not carry answers under its control-plane name.
    pub fn wire_name(&self, surface: Surface) -> &'static str {
        match surface {
            Surface::ControlPlane => self.name,
            Surface::Mcp => self.mcp_name.unwrap_or(self.name),
        }
    }
}

/// The method the control plane calls `name`.
pub fn library_method(name: &str) -> Option<&'static MethodSpec> {
    LIBRARY_METHODS.iter().find(|spec| spec.name == name)
}

/// The method MCP calls `name`.
pub fn library_tool(name: &str) -> Option<&'static MethodSpec> {
    LIBRARY_METHODS
        .iter()
        .find(|spec| spec.mcp_name == Some(name))
}

/// The input schema of `T`: the JSON Schema 2020-12 of what `T`
/// deserializes from.
pub fn input_schema<T: JsonSchema>() -> Map<String, Value> {
    root_schema::<T>(SchemaSettings::draft2020_12())
}

/// The output schema of `T`: the JSON Schema 2020-12 of what `T`
/// serializes to.
pub fn output_schema<T: JsonSchema>() -> Map<String, Value> {
    root_schema::<T>(SchemaSettings::draft2020_12().for_serialize())
}

/// The root schema of `T` under `settings`, without the root `title`,
/// which would only name the Rust type, and without the root
/// `description`, which would only repeat the type's doc comment: a
/// method is described by its summary, and its fields one by one.
fn root_schema<T: JsonSchema>(settings: SchemaSettings) -> Map<String, Value> {
    let schema = settings.into_generator().into_root_schema_for::<T>();
    // A derived schema of a struct is always an object; the empty map
    // stands for any other root, which the table's tests refuse.
    let Value::Object(mut object) = schema.to_value() else {
        return Map::new();
    };
    object.remove("title");
    object.remove("description");
    object
}
