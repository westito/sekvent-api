//! The canonical contract of one service and its JSON form.
//!
//! A contract holds the service, its RPCs and the transitive closure of the
//! messages and enums the RPCs reach through fields, map values and the
//! extensions of those messages. The well-known types protox bundles are
//! referenced by name and never expanded; a user file that merely declares
//! `package google.protobuf` is expanded like any other. Everything is kept
//! in `BTreeMap`s and sorted vectors, so serializing the same protos twice
//! yields identical bytes; declaration order (of fields, enum values or
//! anything else) is not recorded because it never reaches the wire.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context as _, bail};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    ServiceDescriptorProto,
};
use serde::{Deserialize, Serialize};

/// Version of the baseline file format.
pub const FORMAT: u32 = 2;

/// The canonical contract of one service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    /// Always [`FORMAT`].
    pub format: u32,
    /// Full service name, e.g. `billing.v1.Billing`.
    pub service: String,
    /// The `.proto` file declaring the service, relative to its root.
    pub file: String,
    /// RPCs by name.
    pub rpcs: BTreeMap<String, Rpc>,
    /// Messages of the closure by full name.
    pub messages: BTreeMap<String, Message>,
    /// Enums of the closure by full name.
    pub enums: BTreeMap<String, Enum>,
}

/// One RPC of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rpc {
    /// Full name of the request message.
    pub request: String,
    /// Full name of the reply message.
    pub reply: String,
    /// The client sends a stream.
    pub client_streaming: bool,
    /// The server replies with a stream.
    pub server_streaming: bool,
}

/// A message of the closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    /// Fields by number.
    pub fields: BTreeMap<i32, Field>,
    /// Reserved numbers as inclusive `[start, end]` pairs, merged and sorted.
    pub reserved: Vec<[i32; 2]>,
    /// Reserved names, sorted.
    pub reserved_names: Vec<String>,
    /// Extension ranges as inclusive `[start, end]` pairs, merged and sorted.
    pub extension_ranges: Vec<[i32; 2]>,
    /// Extensions of this message declared in the compiled files, by number.
    pub extensions: BTreeMap<i32, Extension>,
}

/// A field of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// Field name.
    pub name: String,
    /// A scalar name, `message:<full name>`, `enum:<full name>` or
    /// `map<K, V>` with `V` in the same grammar.
    #[serde(rename = "type")]
    pub ty: String,
    /// How many values the field carries.
    pub cardinality: Cardinality,
    /// The real oneof holding the field; a proto3 `optional` field's
    /// synthetic oneof is `None`.
    pub oneof: Option<String>,
    /// The explicit proto2 `[default = …]` as written, an enum default as
    /// its number; `None` without one.
    pub default: Option<String>,
}

/// An extension field of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extension {
    /// Full name of the extension, e.g. `billing.v1.note`.
    pub name: String,
    /// The type, in the grammar of [`Field::ty`].
    #[serde(rename = "type")]
    pub ty: String,
    /// How many values the extension carries.
    pub cardinality: Cardinality,
    /// The explicit default, as in [`Field::default`].
    pub default: Option<String>,
}

/// How many values a field carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    /// proto3 implicit or `optional`, proto2 `optional`.
    Singular,
    /// `repeated`.
    Repeated,
    /// `map<K, V>`.
    Map,
    /// proto2 `required`.
    Required,
}

impl fmt::Display for Cardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Singular => "singular",
            Self::Repeated => "repeated",
            Self::Map => "map",
            Self::Required => "required",
        })
    }
}

/// An enum of the closure.
///
/// Values are keyed by number: the wire carries numbers only, so the order
/// in which values are declared is not part of the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enum {
    /// Every name of each number (aliases included), sorted.
    pub values: BTreeMap<i32, Vec<String>>,
    /// Reserved numbers as inclusive `[start, end]` pairs, merged and sorted.
    pub reserved: Vec<[i32; 2]>,
    /// Reserved names, sorted.
    pub reserved_names: Vec<String>,
    /// A proto2 enum: unknown numbers are not kept as enum values.
    pub closed: bool,
}

impl Contract {
    /// The canonical JSON text, with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a contract always serializes");
        text.push('\n');
        text
    }

    /// Parse a baseline written by [`Contract::to_json`].
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let probe: FormatProbe = serde_json::from_str(text).context("not a contract baseline")?;
        if probe.format < FORMAT {
            bail!(
                "outdated contract format {} (expected {FORMAT}); re-emit the baselines with \
                 `cargo sekvent contract emit`",
                probe.format
            );
        }
        if probe.format > FORMAT {
            bail!(
                "unsupported contract format {} (expected {FORMAT})",
                probe.format
            );
        }
        serde_json::from_str(text).context("not a contract baseline")
    }
}

/// The format field alone, read before the rest of a baseline.
#[derive(Deserialize)]
struct FormatProbe {
    format: u32,
}

/// A message or enum definition found in the compiled files.
#[derive(Debug, Clone, Copy)]
enum TypeDef<'a> {
    Message(&'a DescriptorProto),
    Enum {
        proto: &'a EnumDescriptorProto,
        closed: bool,
    },
}

/// Every message, enum and extension of a set of compiled files.
#[derive(Debug)]
pub(crate) struct TypeIndex<'a> {
    /// Messages and enums of the user's files by full name.
    types: BTreeMap<String, TypeDef<'a>>,
    /// Full names of the types in the bundled well-known files.
    bundled: BTreeSet<String>,
    /// Extensions of the user's files with their full names.
    extensions: Vec<(String, &'a FieldDescriptorProto)>,
}

impl<'a> TypeIndex<'a> {
    /// Index `files`, nested types included; the types of the files named
    /// in `bundled` are only recorded as well-known.
    pub(crate) fn new(files: &'a [FileDescriptorProto], bundled: &BTreeSet<String>) -> Self {
        let mut types = BTreeMap::new();
        let mut well_known = BTreeSet::new();
        let mut extensions = Vec::new();
        for file in files {
            if bundled.contains(file.name()) {
                let mut scratch = BTreeMap::new();
                add_types(
                    &mut scratch,
                    &mut Vec::new(),
                    file.package(),
                    &file.message_type,
                    &file.enum_type,
                    true,
                );
                well_known.extend(scratch.into_keys());
                continue;
            }
            let closed = file.syntax() != "proto3";
            for extension in &file.extension {
                extensions.push((qualify(file.package(), extension.name()), extension));
            }
            add_types(
                &mut types,
                &mut extensions,
                file.package(),
                &file.message_type,
                &file.enum_type,
                closed,
            );
        }
        Self {
            types,
            bundled: well_known,
            extensions,
        }
    }

    /// The contract of `service`, declared in `file`.
    pub(crate) fn service_contract(
        &self,
        file: &FileDescriptorProto,
        service: &ServiceDescriptorProto,
    ) -> anyhow::Result<Contract> {
        let mut pending = Vec::new();
        let mut rpcs = BTreeMap::new();
        for method in &service.method {
            let rpc = Rpc {
                request: type_name(method.input_type()),
                reply: type_name(method.output_type()),
                client_streaming: method.client_streaming(),
                server_streaming: method.server_streaming(),
            };
            pending.push(rpc.request.clone());
            pending.push(rpc.reply.clone());
            rpcs.insert(method.name().to_owned(), rpc);
        }
        let mut contract = Contract {
            format: FORMAT,
            service: qualify(file.package(), service.name()),
            file: file.name().to_owned(),
            rpcs,
            messages: BTreeMap::new(),
            enums: BTreeMap::new(),
        };
        loop {
            while let Some(name) = pending.pop() {
                if self.bundled.contains(&name)
                    || contract.messages.contains_key(&name)
                    || contract.enums.contains_key(&name)
                {
                    continue;
                }
                match self.get(&name)? {
                    TypeDef::Message(proto) => {
                        let message = self.message(proto, &mut pending)?;
                        contract.messages.insert(name, message);
                    }
                    TypeDef::Enum { proto, closed } => {
                        contract.enums.insert(name, enum_contract(proto, closed));
                    }
                }
            }
            self.add_extensions(&mut contract.messages, &mut pending)?;
            if pending.is_empty() {
                return Ok(contract);
            }
        }
    }

    /// Record every extension of a message in `messages` that is not yet
    /// recorded; the types they reach join `pending`.
    fn add_extensions(
        &self,
        messages: &mut BTreeMap<String, Message>,
        pending: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        for (name, proto) in &self.extensions {
            let Some(message) = messages.get_mut(&type_name(proto.extendee())) else {
                continue;
            };
            if message.extensions.contains_key(&proto.number()) {
                continue;
            }
            let (ty, cardinality) = self.field_type(proto, pending)?;
            message.extensions.insert(
                proto.number(),
                Extension {
                    name: name.clone(),
                    ty,
                    cardinality,
                    default: self.default_value(proto),
                },
            );
        }
        Ok(())
    }

    fn get(&self, name: &str) -> anyhow::Result<TypeDef<'a>> {
        self.types
            .get(name)
            .copied()
            .with_context(|| format!("type `{name}` is not defined in the compiled files"))
    }

    fn message(
        &self,
        proto: &DescriptorProto,
        pending: &mut Vec<String>,
    ) -> anyhow::Result<Message> {
        let mut fields = BTreeMap::new();
        for field in &proto.field {
            let (ty, cardinality) = self.field_type(field, pending)?;
            fields.insert(
                field.number(),
                Field {
                    name: field.name().to_owned(),
                    ty,
                    cardinality,
                    oneof: real_oneof(proto, field),
                    default: self.default_value(field),
                },
            );
        }
        let reserved = proto
            .reserved_range
            .iter()
            .map(|range| [range.start(), range.end() - 1])
            .collect();
        let extension_ranges = proto
            .extension_range
            .iter()
            .map(|range| [range.start(), range.end() - 1])
            .collect();
        Ok(Message {
            fields,
            reserved: merge_ranges(reserved),
            reserved_names: sorted(&proto.reserved_name),
            extension_ranges: merge_ranges(extension_ranges),
            extensions: BTreeMap::new(),
        })
    }

    /// A field's explicit default; an enum default becomes its number, so
    /// renaming the value stays compatible.
    fn default_value(&self, field: &FieldDescriptorProto) -> Option<String> {
        let text = field.default_value.as_deref()?;
        if field.r#type() == Type::Enum
            && let Some(TypeDef::Enum { proto, .. }) = self.types.get(&type_name(field.type_name()))
            && let Some(value) = proto.value.iter().find(|value| value.name() == text)
        {
            return Some(value.number().to_string());
        }
        Some(text.to_owned())
    }

    fn field_type(
        &self,
        field: &FieldDescriptorProto,
        pending: &mut Vec<String>,
    ) -> anyhow::Result<(String, Cardinality)> {
        if let Some(entry) = self.map_entry(field) {
            let part = |number: i32| {
                entry
                    .field
                    .iter()
                    .find(|part| part.number() == number)
                    .with_context(|| format!("map field `{}` lacks part {number}", field.name()))
            };
            let key = value_type(part(1)?, pending);
            let value = value_type(part(2)?, pending);
            return Ok((format!("map<{key}, {value}>"), Cardinality::Map));
        }
        let cardinality = match field.label() {
            Label::Optional => Cardinality::Singular,
            Label::Repeated => Cardinality::Repeated,
            Label::Required => Cardinality::Required,
        };
        Ok((value_type(field, pending), cardinality))
    }

    /// The synthesized entry message of a map field.
    fn map_entry(&self, field: &FieldDescriptorProto) -> Option<&'a DescriptorProto> {
        if field.label() != Label::Repeated || field.r#type() != Type::Message {
            return None;
        }
        match self.types.get(&type_name(field.type_name())) {
            Some(TypeDef::Message(entry)) if is_map_entry(entry) => Some(entry),
            _ => None,
        }
    }
}

fn add_types<'a>(
    types: &mut BTreeMap<String, TypeDef<'a>>,
    extensions: &mut Vec<(String, &'a FieldDescriptorProto)>,
    prefix: &str,
    messages: &'a [DescriptorProto],
    enums: &'a [EnumDescriptorProto],
    closed: bool,
) {
    for proto in enums {
        types.insert(
            qualify(prefix, proto.name()),
            TypeDef::Enum { proto, closed },
        );
    }
    for proto in messages {
        let name = qualify(prefix, proto.name());
        for extension in &proto.extension {
            extensions.push((qualify(&name, extension.name()), extension));
        }
        add_types(
            types,
            extensions,
            &name,
            &proto.nested_type,
            &proto.enum_type,
            closed,
        );
        types.insert(name, TypeDef::Message(proto));
    }
}

fn is_map_entry(message: &DescriptorProto) -> bool {
    message
        .options
        .as_ref()
        .is_some_and(prost_types::MessageOptions::map_entry)
}

/// `prefix.name`, or `name` alone for an empty prefix.
pub(crate) fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// A descriptor's type reference without its leading dot.
fn type_name(reference: &str) -> String {
    reference.strip_prefix('.').unwrap_or(reference).to_owned()
}

/// The type of a single value; message and enum types join `pending`.
fn value_type(field: &FieldDescriptorProto, pending: &mut Vec<String>) -> String {
    match field.r#type() {
        Type::Message => {
            let name = type_name(field.type_name());
            pending.push(name.clone());
            format!("message:{name}")
        }
        Type::Enum => {
            let name = type_name(field.type_name());
            pending.push(name.clone());
            format!("enum:{name}")
        }
        scalar => scalar_name(scalar).to_owned(),
    }
}

/// The proto spelling of a field type.
fn scalar_name(ty: Type) -> &'static str {
    match ty {
        Type::Double => "double",
        Type::Float => "float",
        Type::Int64 => "int64",
        Type::Uint64 => "uint64",
        Type::Int32 => "int32",
        Type::Fixed64 => "fixed64",
        Type::Fixed32 => "fixed32",
        Type::Bool => "bool",
        Type::String => "string",
        Type::Group => "group",
        Type::Message => "message",
        Type::Bytes => "bytes",
        Type::Uint32 => "uint32",
        Type::Enum => "enum",
        Type::Sfixed32 => "sfixed32",
        Type::Sfixed64 => "sfixed64",
        Type::Sint32 => "sint32",
        Type::Sint64 => "sint64",
    }
}

fn real_oneof(message: &DescriptorProto, field: &FieldDescriptorProto) -> Option<String> {
    if field.proto3_optional() {
        return None;
    }
    let index = usize::try_from(field.oneof_index?).ok()?;
    message
        .oneof_decl
        .get(index)
        .map(|oneof| oneof.name().to_owned())
}

fn enum_contract(proto: &EnumDescriptorProto, closed: bool) -> Enum {
    let mut values: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    for value in &proto.value {
        values
            .entry(value.number())
            .or_default()
            .push(value.name().to_owned());
    }
    for names in values.values_mut() {
        names.sort();
    }
    let reserved = proto
        .reserved_range
        .iter()
        .map(|range| [range.start(), range.end()])
        .collect();
    Enum {
        values,
        reserved: merge_ranges(reserved),
        reserved_names: sorted(&proto.reserved_name),
        closed,
    }
}

fn sorted(names: &[String]) -> Vec<String> {
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    names
}

/// Sort inclusive ranges and merge the overlapping and adjacent ones.
pub(crate) fn merge_ranges(mut ranges: Vec<[i32; 2]>) -> Vec<[i32; 2]> {
    ranges.sort_unstable();
    let mut merged: Vec<[i32; 2]> = Vec::with_capacity(ranges.len());
    for [start, end] in ranges {
        match merged.last_mut() {
            Some(last) if last[1].saturating_add(1) >= start => last[1] = last[1].max(end),
            _ => merged.push([start, end]),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::testing::contracts;

    const MONEY: &str = r#"syntax = "proto3";
package common.v1;

message Money {
  string currency = 1;
  int64 units = 2;
  int32 nanos = 10;
}
"#;

    const SHOP: &str = r#"syntax = "proto3";
package shop.v1;

import "common/v1/money.proto";
import "google/protobuf/timestamp.proto";

// Comments and options are not part of the contract.
service Shop {
  rpc Place(PlaceRequest) returns (PlaceReply);
  rpc Watch(PlaceRequest) returns (stream PlaceReply);
}

message PlaceRequest {
  message Line {
    string sku = 1;
    uint32 quantity = 2;
  }
  repeated Line lines = 1;
  map<string, common.v1.Money> prices = 2;
  optional string note = 3;
  oneof payment {
    string card = 4;
    string voucher = 5;
  }
  google.protobuf.Timestamp at = 6 [deprecated = true];
  reserved 7, 8, 10 to 12;
  reserved "coupon";
}

message PlaceReply {
  Status status = 1;
}

enum Status {
  option allow_alias = true;
  STATUS_UNSPECIFIED = 0;
  STATUS_PLACED = 1;
  STATUS_ACCEPTED = 1;
  reserved 3;
}

message Unreachable {
  string id = 1;
}
"#;

    /// The expected baseline; `@NAME@` stands for a message's fields.
    const SHOP_JSON: &str = r#"{
  "format": 2,
  "service": "shop.v1.Shop",
  "file": "shop/v1/shop.proto",
  "rpcs": {
    "Place": {
      "request": "shop.v1.PlaceRequest",
      "reply": "shop.v1.PlaceReply",
      "client_streaming": false,
      "server_streaming": false
    },
    "Watch": {
      "request": "shop.v1.PlaceRequest",
      "reply": "shop.v1.PlaceReply",
      "client_streaming": false,
      "server_streaming": true
    }
  },
  "messages": {
    "common.v1.Money": {
      "fields": {
@MONEY@
      },
      "reserved": [],
      "reserved_names": [],
      "extension_ranges": [],
      "extensions": {}
    },
    "shop.v1.PlaceReply": {
      "fields": {
@REPLY@
      },
      "reserved": [],
      "reserved_names": [],
      "extension_ranges": [],
      "extensions": {}
    },
    "shop.v1.PlaceRequest": {
      "fields": {
@REQUEST@
      },
      "reserved": [
        [
          7,
          8
        ],
        [
          10,
          12
        ]
      ],
      "reserved_names": [
        "coupon"
      ],
      "extension_ranges": [],
      "extensions": {}
    },
    "shop.v1.PlaceRequest.Line": {
      "fields": {
@LINE@
      },
      "reserved": [],
      "reserved_names": [],
      "extension_ranges": [],
      "extensions": {}
    }
  },
  "enums": {
    "shop.v1.Status": {
      "values": {
        "0": [
          "STATUS_UNSPECIFIED"
        ],
        "1": [
          "STATUS_ACCEPTED",
          "STATUS_PLACED"
        ]
      },
      "reserved": [
        [
          3,
          3
        ]
      ],
      "reserved_names": [],
      "closed": false
    }
  }
}
"#;

    /// Pretty-printed fields `(number, name, type, cardinality, oneof)`.
    fn fields(list: &[(u8, &str, &str, &str, &str)]) -> String {
        list.iter()
            .map(|(number, name, ty, cardinality, oneof)| {
                [
                    format!("        \"{number}\": {{"),
                    format!("          \"name\": \"{name}\","),
                    format!("          \"type\": \"{ty}\","),
                    format!("          \"cardinality\": \"{cardinality}\","),
                    format!("          \"oneof\": {oneof},"),
                    "          \"default\": null".to_owned(),
                    "        }".to_owned(),
                ]
                .join("\n")
            })
            .collect::<Vec<_>>()
            .join(",\n")
    }

    fn expected_shop() -> String {
        let one = "singular";
        let money = fields(&[
            (1, "currency", "string", one, "null"),
            (2, "units", "int64", one, "null"),
            (10, "nanos", "int32", one, "null"),
        ]);
        let reply = fields(&[(1, "status", "enum:shop.v1.Status", one, "null")]);
        let request = fields(&[
            (
                1,
                "lines",
                "message:shop.v1.PlaceRequest.Line",
                "repeated",
                "null",
            ),
            (
                2,
                "prices",
                "map<string, message:common.v1.Money>",
                "map",
                "null",
            ),
            (3, "note", "string", one, "null"),
            (4, "card", "string", one, "\"payment\""),
            (5, "voucher", "string", one, "\"payment\""),
            (6, "at", "message:google.protobuf.Timestamp", one, "null"),
        ]);
        let line = fields(&[
            (1, "sku", "string", one, "null"),
            (2, "quantity", "uint32", one, "null"),
        ]);
        SHOP_JSON
            .replace("@MONEY@", &money)
            .replace("@REPLY@", &reply)
            .replace("@REQUEST@", &request)
            .replace("@LINE@", &line)
    }

    fn shop() -> Contract {
        let mut all = contracts(&[
            ("common/v1/money.proto", MONEY),
            ("shop/v1/shop.proto", SHOP),
        ]);
        assert_eq!(all.len(), 1, "{:?}", all.keys());
        all.remove("shop.v1.Shop").unwrap()
    }

    #[test]
    fn the_canonical_json_matches_the_fixture_byte_for_byte() {
        let contract = shop();
        assert_eq!(contract.to_json(), expected_shop());
    }

    #[test]
    fn a_baseline_round_trips_byte_for_byte() {
        let json = shop().to_json();
        let parsed = Contract::from_json(&json).unwrap();
        assert_eq!(parsed, shop());
        assert_eq!(parsed.to_json(), json);
    }

    #[test]
    fn foreign_outdated_or_future_baselines_are_rejected() {
        let error = Contract::from_json("{}").unwrap_err();
        assert!(
            format!("{error:#}").contains("not a contract baseline"),
            "{error:#}"
        );
        let future = shop().to_json().replace("\"format\": 2", "\"format\": 3");
        let error = Contract::from_json(&future).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unsupported contract format 3 (expected 2)"
        );
        let outdated = shop().to_json().replace("\"format\": 2", "\"format\": 1");
        let error = Contract::from_json(&outdated).unwrap_err();
        assert_eq!(
            error.to_string(),
            "outdated contract format 1 (expected 2); re-emit the baselines with `cargo \
             sekvent contract emit`"
        );
        let extra =
            shop()
                .to_json()
                .replacen("\"format\": 2,", "\"format\": 2,\n  \"extra\": true,", 1);
        let error = Contract::from_json(&extra).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a contract baseline"),
            "{error:#}"
        );
    }

    #[test]
    fn proto2_fields_and_enums_keep_their_labels_and_closedness() {
        let mut all = contracts(&[(
            "legacy.proto",
            r#"syntax = "proto2";
package legacy.v1;
service Legacy { rpc Get(GetRequest) returns (GetReply); }
message GetRequest {
  required string id = 1;
  optional sint64 limit = 2;
  repeated Level levels = 3;
  reserved 5 to max;
}
message GetReply { optional bytes body = 1; }
enum Level { LOW = 0; HIGH = 1; reserved 7 to max; }
"#,
        )]);
        let contract = all.remove("legacy.v1.Legacy").unwrap();
        assert_eq!(contract.file, "legacy.proto");
        let request = &contract.messages["legacy.v1.GetRequest"];
        let cardinalities: Vec<(Cardinality, &str)> = request
            .fields
            .values()
            .map(|field| (field.cardinality, field.ty.as_str()))
            .collect();
        assert_eq!(
            cardinalities,
            [
                (Cardinality::Required, "string"),
                (Cardinality::Singular, "sint64"),
                (Cardinality::Repeated, "enum:legacy.v1.Level"),
            ]
        );
        assert_eq!(request.reserved, [[5, 536_870_911]]);
        let level = &contract.enums["legacy.v1.Level"];
        assert!(level.closed);
        assert_eq!(level.reserved, [[7, i32::MAX]]);
        assert_eq!(
            contract.messages["legacy.v1.GetReply"].fields[&1].ty,
            "bytes"
        );
    }

    #[test]
    fn extensions_defaults_and_extension_ranges_are_recorded() {
        let mut all = contracts(&[(
            "ext.proto",
            r#"syntax = "proto2";
package ext.v1;
service Svc { rpc Get(Req) returns (Req); }
message Req {
  optional int32 limit = 1 [default = -5];
  optional Mode mode = 2 [default = FAST];
  optional string plain = 3;
  extensions 100 to 199, 500 to max;
}
enum Mode { SLOW = 0; FAST = 1; }
message Extra { optional string x = 1; }
message Holder {
  extend Req { optional Extra extra = 101; }
}
extend Req { repeated bool flags = 100; }
message Unrelated { extensions 1 to 5; }
extend Unrelated { optional int32 u = 1 [default = 7]; }
"#,
        )]);
        let contract = all.remove("ext.v1.Svc").unwrap();
        let request = &contract.messages["ext.v1.Req"];
        let defaults: Vec<Option<&str>> = request
            .fields
            .values()
            .map(|field| field.default.as_deref())
            .collect();
        assert_eq!(defaults, [Some("-5"), Some("1"), None]);
        assert_eq!(request.extension_ranges, [[100, 199], [500, 536_870_911]]);
        assert_eq!(
            request.extensions,
            BTreeMap::from([
                (
                    100,
                    Extension {
                        name: "ext.v1.flags".into(),
                        ty: "bool".into(),
                        cardinality: Cardinality::Repeated,
                        default: None,
                    }
                ),
                (
                    101,
                    Extension {
                        name: "ext.v1.Holder.extra".into(),
                        ty: "message:ext.v1.Extra".into(),
                        cardinality: Cardinality::Singular,
                        default: None,
                    }
                ),
            ])
        );
        assert_eq!(
            contract.messages.keys().collect::<Vec<_>>(),
            ["ext.v1.Extra", "ext.v1.Req"],
            "an extension's type joins the closure; the declaring and unrelated messages do not"
        );
        assert!(contract.messages["ext.v1.Extra"].extensions.is_empty());

        let json = contract.to_json();
        assert_eq!(Contract::from_json(&json).unwrap(), contract);
    }

    #[test]
    fn well_known_requests_and_enum_maps_are_not_expanded() {
        let mut all = contracts(&[(
            "svc.proto",
            r#"syntax = "proto3";
import "google/protobuf/empty.proto";
service Pinger { rpc Ping(google.protobuf.Empty) returns (Pong); }
message Pong { map<int32, Mood> moods = 1; Pong next = 2; }
enum Mood { MOOD_UNSPECIFIED = 0; }
"#,
        )]);
        let contract = all.remove("Pinger").unwrap();
        assert_eq!(contract.rpcs["Ping"].request, "google.protobuf.Empty");
        assert_eq!(
            contract.messages.keys().collect::<Vec<_>>(),
            ["Pong"],
            "the map entry and the well-known type stay out"
        );
        assert_eq!(
            contract.messages["Pong"].fields[&1].ty,
            "map<int32, enum:Mood>"
        );
        assert_eq!(contract.messages["Pong"].fields[&2].ty, "message:Pong");
        assert!(contract.enums.contains_key("Mood"));
    }

    #[test]
    fn every_field_type_has_its_proto_spelling() {
        let all = [
            (Type::Double, "double"),
            (Type::Float, "float"),
            (Type::Int64, "int64"),
            (Type::Uint64, "uint64"),
            (Type::Int32, "int32"),
            (Type::Fixed64, "fixed64"),
            (Type::Fixed32, "fixed32"),
            (Type::Bool, "bool"),
            (Type::String, "string"),
            (Type::Group, "group"),
            (Type::Message, "message"),
            (Type::Bytes, "bytes"),
            (Type::Uint32, "uint32"),
            (Type::Enum, "enum"),
            (Type::Sfixed32, "sfixed32"),
            (Type::Sfixed64, "sfixed64"),
            (Type::Sint32, "sint32"),
            (Type::Sint64, "sint64"),
        ];
        for (ty, name) in all {
            assert_eq!(scalar_name(ty), name);
        }
        for (cardinality, name) in [
            (Cardinality::Singular, "singular"),
            (Cardinality::Repeated, "repeated"),
            (Cardinality::Map, "map"),
            (Cardinality::Required, "required"),
        ] {
            assert_eq!(cardinality.to_string(), name);
        }
    }

    #[test]
    fn ranges_are_sorted_and_merged() {
        assert_eq!(
            merge_ranges(vec![[10, 12], [1, 1], [2, 3], [5, 6], [6, 8], [11, 11]]),
            [[1, 3], [5, 8], [10, 12]]
        );
        assert_eq!(
            merge_ranges(vec![[i32::MAX, i32::MAX], [1, i32::MAX]]),
            [[1, i32::MAX]]
        );
        assert!(merge_ranges(Vec::new()).is_empty());
        assert_eq!(qualify("", "A"), "A");
        assert_eq!(qualify("a.b", "C"), "a.b.C");
    }

    #[test]
    fn a_dangling_reference_or_a_broken_map_entry_is_an_error() {
        use prost_types::{MessageOptions, MethodDescriptorProto};

        let file = FileDescriptorProto {
            name: Some("x.proto".into()),
            service: vec![ServiceDescriptorProto {
                name: Some("S".into()),
                method: vec![MethodDescriptorProto {
                    name: Some("M".into()),
                    input_type: Some(".Missing".into()),
                    output_type: Some(".Missing".into()),
                    ..MethodDescriptorProto::default()
                }],
                ..ServiceDescriptorProto::default()
            }],
            ..FileDescriptorProto::default()
        };
        let files = [file];
        let index = TypeIndex::new(&files, &BTreeSet::new());
        let error = index
            .service_contract(&files[0], &files[0].service[0])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "type `Missing` is not defined in the compiled files"
        );

        let entry = DescriptorProto {
            name: Some("Entry".into()),
            options: Some(MessageOptions {
                map_entry: Some(true),
                ..MessageOptions::default()
            }),
            ..DescriptorProto::default()
        };
        let holder = DescriptorProto {
            name: Some("Holder".into()),
            field: vec![FieldDescriptorProto {
                name: Some("pairs".into()),
                number: Some(1),
                label: Some(Label::Repeated.into()),
                r#type: Some(Type::Message.into()),
                type_name: Some(".Holder.Entry".into()),
                ..FieldDescriptorProto::default()
            }],
            nested_type: vec![entry],
            ..DescriptorProto::default()
        };
        let files = [FileDescriptorProto {
            message_type: vec![holder],
            ..FileDescriptorProto::default()
        }];
        let index = TypeIndex::new(&files, &BTreeSet::new());
        let error = index
            .message(&files[0].message_type[0], &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.to_string(), "map field `pairs` lacks part 1");
    }
}
