//! Wire-compatibility rules between a baseline and the current contract.
//!
//! A change is breaking when an existing binary peer could misread or fail
//! on the wire. Renamed fields, enum values and oneofs are compatible: the
//! binary encoding carries numbers only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::model::{Contract, Enum, Message};

/// A breaking change of one service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Full service name.
    pub service: String,
    /// What changed.
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "breaking: {}: {}", self.service, self.message)
    }
}

/// The breaking changes from `baseline` to `current`, sorted; `None` means
/// the service no longer exists.
pub fn compare(baseline: &Contract, current: Option<&Contract>) -> Vec<Finding> {
    let mut out = Vec::new();
    match current {
        None => out
            .push("the service is gone (removed, renamed or moved to another package)".to_owned()),
        Some(current) => {
            rpcs(baseline, current, &mut out);
            for (name, old) in &baseline.messages {
                if let Some(new) = current.messages.get(name) {
                    message(name, old, new, &mut out);
                }
            }
            for (name, old) in &baseline.enums {
                if let Some(new) = current.enums.get(name) {
                    enumeration(name, old, new, &mut out);
                }
            }
        }
    }
    let mut findings: Vec<Finding> = out
        .into_iter()
        .map(|message| Finding {
            service: baseline.service.clone(),
            message,
        })
        .collect();
    findings.sort();
    findings
}

fn rpcs(baseline: &Contract, current: &Contract, out: &mut Vec<String>) {
    for (name, old) in &baseline.rpcs {
        let Some(new) = current.rpcs.get(name) else {
            out.push(format!("rpc {name} was removed"));
            continue;
        };
        for (what, before, now) in [
            ("request", &old.request, &new.request),
            ("reply", &old.reply, &new.reply),
        ] {
            if before != now {
                out.push(format!(
                    "rpc {name} {what} type changed from {before} to {now}"
                ));
            }
        }
        for (what, before, now) in [
            ("client", old.client_streaming, new.client_streaming),
            ("server", old.server_streaming, new.server_streaming),
        ] {
            if before != now {
                out.push(format!(
                    "rpc {name} {what} streaming changed from {before} to {now}"
                ));
            }
        }
    }
}

fn message(name: &str, old: &Message, new: &Message, out: &mut Vec<String>) {
    for (number, field) in &old.fields {
        let label = format!("field {number} ({}) of {name}", field.name);
        let Some(now) = new.fields.get(number) else {
            if !covers(&new.reserved, *number) {
                out.push(format!("{label} was removed without reserving its number"));
            }
            continue;
        };
        if field.ty != now.ty {
            out.push(format!(
                "{label} changed type from {} to {}",
                field.ty, now.ty
            ));
        }
        if field.cardinality != now.cardinality {
            out.push(format!(
                "{label} changed cardinality from {} to {}",
                field.cardinality, now.cardinality
            ));
        }
        match (&field.oneof, &now.oneof) {
            (None, Some(oneof)) => out.push(format!("{label} moved into oneof {oneof}")),
            (Some(oneof), None) => out.push(format!("{label} moved out of oneof {oneof}")),
            _ => {}
        }
    }
    oneof_members(name, old, new, out);
    reserved(name, &old.reserved, &new.reserved, out);
}

/// Fields that shared a oneof must still share one, and no other field that
/// existed before may have joined it.
fn oneof_members(name: &str, old: &Message, new: &Message, out: &mut Vec<String>) {
    let common: Vec<(i32, &str, &str)> = old
        .fields
        .iter()
        .filter_map(|(number, field)| {
            let before = field.oneof.as_deref()?;
            let now = new.fields.get(number)?.oneof.as_deref()?;
            Some((*number, before, now))
        })
        .collect();
    let mut before_groups: BTreeMap<&str, BTreeSet<i32>> = BTreeMap::new();
    let mut now_groups: BTreeMap<&str, BTreeSet<i32>> = BTreeMap::new();
    for (number, before, now) in &common {
        before_groups.entry(*before).or_default().insert(*number);
        now_groups.entry(*now).or_default().insert(*number);
    }
    for (oneof, members) in &before_groups {
        let now_names: BTreeSet<&str> = common
            .iter()
            .filter(|(_, before, _)| before == oneof)
            .map(|(_, _, now)| *now)
            .collect();
        let together: BTreeSet<i32> = now_names
            .iter()
            .flat_map(|now| now_groups[now].iter().copied())
            .collect();
        if now_names.len() > 1 || &together != members {
            out.push(format!(
                "oneof {oneof} of {name} no longer holds the same existing fields"
            ));
        }
    }
}

fn enumeration(name: &str, old: &Enum, new: &Enum, out: &mut Vec<String>) {
    for (number, names) in &old.values {
        if !new.values.contains_key(number) && !covers(&new.reserved, *number) {
            out.push(format!(
                "value {number} ({}) of {name} was removed without reserving its number",
                names.join(", ")
            ));
        }
    }
    reserved(name, &old.reserved, &new.reserved, out);
    if old.closed != new.closed {
        out.push(format!(
            "enum {name} changed from {} to {}",
            openness(old.closed),
            openness(new.closed)
        ));
    }
}

fn openness(closed: bool) -> &'static str {
    if closed { "closed" } else { "open" }
}

/// Numbers reserved in `old` must stay reserved in `new`.
fn reserved(name: &str, old: &[[i32; 2]], new: &[[i32; 2]], out: &mut Vec<String>) {
    for range in old {
        for [start, end] in uncovered(*range, new) {
            if start == end {
                out.push(format!(
                    "reserved number {start} of {name} is no longer reserved"
                ));
            } else {
                out.push(format!(
                    "reserved numbers {start} to {end} of {name} are no longer reserved"
                ));
            }
        }
    }
}

fn covers(ranges: &[[i32; 2]], number: i32) -> bool {
    ranges
        .iter()
        .any(|[start, end]| (*start..=*end).contains(&number))
}

/// The parts of the inclusive `range` that the sorted, merged `ranges` do
/// not cover.
fn uncovered([start, end]: [i32; 2], ranges: &[[i32; 2]]) -> Vec<[i32; 2]> {
    let mut gaps = Vec::new();
    let mut next = Some(start);
    for &[low, high] in ranges {
        let Some(from) = next else { break };
        if high < from {
            continue;
        }
        if low > end {
            break;
        }
        if low > from {
            gaps.push([from, low - 1]);
        }
        next = high.checked_add(1);
    }
    if let Some(from) = next
        && from <= end
    {
        gaps.push([from, end]);
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::testing::contracts;

    /// The contract of service `t.v1.Svc` in a proto3 file with `body`.
    fn svc(body: &str) -> Contract {
        svc_with("proto3", body)
    }

    fn svc_with(syntax: &str, body: &str) -> Contract {
        let source = format!("syntax = \"{syntax}\";\npackage t.v1;\n{body}\n");
        contracts(&[("t.proto", source.as_str())])
            .remove("t.v1.Svc")
            .expect("the fixture declares t.v1.Svc")
    }

    fn breaking(old: &str, new: &str) -> Vec<String> {
        compare(&svc(old), Some(&svc(new)))
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn assert_compatible(old: &str, new: &str) {
        assert_eq!(breaking(old, new), Vec::<String>::new(), "{old}\n=>\n{new}");
    }

    const SERVICE: &str = "service Svc { rpc Get(Req) returns (Rep); }\nmessage Rep {}\n";

    fn with_req(req: &str) -> String {
        format!("{SERVICE}message Req {{ {req} }}\n")
    }

    const P: &str = "breaking: t.v1.Svc: ";

    #[test]
    fn r1_a_missing_service_is_breaking() {
        let findings = compare(&svc(&with_req("")), None);
        assert_eq!(
            findings.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [format!(
                "{P}the service is gone (removed, renamed or moved to another package)"
            )]
        );
    }

    #[test]
    fn r2_a_removed_rpc_is_breaking_and_a_new_one_is_not() {
        let two = "service Svc { rpc Get(Req) returns (Rep); rpc Put(Req) returns (Rep); }\n\
                   message Req {}\nmessage Rep {}\n";
        let one = "service Svc { rpc Get(Req) returns (Rep); }\nmessage Req {}\nmessage Rep {}\n";
        assert_eq!(breaking(two, one), [format!("{P}rpc Put was removed")]);
        assert_compatible(one, two);
    }

    #[test]
    fn r3_changed_request_or_reply_types_are_breaking() {
        let base = "service Svc { rpc Get(Req) returns (Rep); }\nmessage Req {}\nmessage Rep {}\n\
                    message Other {}\n";
        let changed = "service Svc { rpc Get(Other) returns (Req); }\nmessage Req {}\n\
                       message Rep {}\nmessage Other {}\n";
        assert_eq!(
            breaking(base, changed),
            [
                format!("{P}rpc Get reply type changed from t.v1.Rep to t.v1.Req"),
                format!("{P}rpc Get request type changed from t.v1.Req to t.v1.Other"),
            ]
        );
    }

    #[test]
    fn r4_changed_streaming_flags_are_breaking() {
        let base = with_req("");
        let streaming = "service Svc { rpc Get(stream Req) returns (stream Rep); }\n\
                         message Rep {}\nmessage Req {}\n";
        assert_eq!(
            breaking(&base, streaming),
            [
                format!("{P}rpc Get client streaming changed from false to true"),
                format!("{P}rpc Get server streaming changed from false to true"),
            ]
        );
    }

    #[test]
    fn r5_a_removed_field_must_leave_its_number_reserved() {
        let base = with_req("string id = 1; uint32 quantity = 3;");
        assert_eq!(
            breaking(&base, &with_req("string id = 1;")),
            [format!(
                "{P}field 3 (quantity) of t.v1.Req was removed without reserving its number"
            )]
        );
        assert_compatible(&base, &with_req("string id = 1; reserved 3;"));
        assert_compatible(&base, &with_req("string id = 1; reserved 2 to 4;"));
    }

    #[test]
    fn r6_a_changed_field_type_is_breaking() {
        let base = with_req("string id = 1; map<string, int32> tags = 2; Rep rep = 3;");
        let changed = format!(
            "{SERVICE}message Req {{ bytes id = 1; map<string, int64> tags = 2; Other rep = 3; }}\n\
             message Other {{}}\n"
        );
        assert_eq!(
            breaking(&base, &changed),
            [
                format!("{P}field 1 (id) of t.v1.Req changed type from string to bytes"),
                format!(
                    "{P}field 2 (tags) of t.v1.Req changed type from map<string, int32> to \
                     map<string, int64>"
                ),
                format!(
                    "{P}field 3 (rep) of t.v1.Req changed type from message:t.v1.Rep to \
                     message:t.v1.Other"
                ),
            ]
        );
    }

    #[test]
    fn r7_a_changed_cardinality_is_breaking() {
        let base = with_req("string id = 1; repeated string tags = 2;");
        let changed = with_req("repeated string id = 1; string tags = 2;");
        assert_eq!(
            breaking(&base, &changed),
            [
                format!(
                    "{P}field 1 (id) of t.v1.Req changed cardinality from singular to repeated"
                ),
                format!(
                    "{P}field 2 (tags) of t.v1.Req changed cardinality from repeated to singular"
                ),
            ]
        );
        assert_compatible(
            &with_req("string id = 1;"),
            &with_req("optional string id = 1;"),
        );
    }

    #[test]
    fn r8_moving_fields_across_oneofs_is_breaking() {
        let base = with_req("oneof pick { string a = 1; string b = 2; } string c = 3;");
        assert_eq!(
            breaking(
                &base,
                &with_req("oneof pick { string a = 1; string c = 3; } string b = 2;")
            ),
            [
                format!("{P}field 2 (b) of t.v1.Req moved out of oneof pick"),
                format!("{P}field 3 (c) of t.v1.Req moved into oneof pick"),
            ]
        );
        assert_eq!(
            breaking(
                &base,
                &with_req("oneof x { string a = 1; } oneof y { string b = 2; } string c = 3;")
            ),
            [format!(
                "{P}oneof pick of t.v1.Req no longer holds the same existing fields"
            )]
        );
        let two = with_req("oneof x { string a = 1; } oneof y { string b = 2; }");
        assert_eq!(
            breaking(&two, &with_req("oneof xy { string a = 1; string b = 2; }")),
            [
                format!("{P}oneof x of t.v1.Req no longer holds the same existing fields"),
                format!("{P}oneof y of t.v1.Req no longer holds the same existing fields"),
            ]
        );
    }

    #[test]
    fn r8_renaming_a_oneof_or_adding_a_new_member_is_compatible() {
        let base = with_req("oneof pick { string a = 1; string b = 2; }");
        assert_compatible(
            &base,
            &with_req("oneof choice { string a = 1; string b = 2; string d = 4; }"),
        );
        assert_compatible(&base, &with_req("oneof pick { string a = 1; } reserved 2;"));
    }

    #[test]
    fn r9_reserved_message_numbers_must_stay_reserved() {
        let base = with_req("string id = 1; reserved 2, 5 to 9;");
        assert_eq!(
            breaking(&base, &with_req("string id = 1; reserved 6 to 7;")),
            [
                format!("{P}reserved number 2 of t.v1.Req is no longer reserved"),
                format!("{P}reserved number 5 of t.v1.Req is no longer reserved"),
                format!("{P}reserved numbers 8 to 9 of t.v1.Req are no longer reserved"),
            ]
        );
        assert_eq!(
            breaking(
                &base,
                &with_req("string id = 1; string back = 2; reserved 5 to 9;")
            ),
            [format!(
                "{P}reserved number 2 of t.v1.Req is no longer reserved"
            )]
        );
        assert_compatible(&base, &with_req("string id = 1; reserved 2 to 10;"));
    }

    fn with_enum(values: &str) -> String {
        format!(
            "{SERVICE}message Req {{ Kind kind = 1; }}\nenum Kind {{ KIND_UNSPECIFIED = 0; {values} }}\n"
        )
    }

    #[test]
    fn r10_a_removed_enum_value_must_leave_its_number_reserved() {
        let base = with_enum("KIND_A = 1; KIND_B = 2;");
        assert_eq!(
            breaking(&base, &with_enum("KIND_A = 1;")),
            [format!(
                "{P}value 2 (KIND_B) of t.v1.Kind was removed without reserving its number"
            )]
        );
        assert_compatible(&base, &with_enum("KIND_A = 1; reserved 2;"));
        assert_compatible(&base, &with_enum("KIND_FIRST = 1; KIND_B = 2; KIND_C = 3;"));
    }

    #[test]
    fn r11_reserved_enum_numbers_must_stay_reserved() {
        let base = with_enum("reserved 4 to 6;");
        assert_eq!(
            breaking(&base, &with_enum("KIND_D = 4; reserved 5 to 6;")),
            [format!(
                "{P}reserved number 4 of t.v1.Kind is no longer reserved"
            )]
        );
        assert_eq!(
            breaking(&base, &with_enum("")),
            [format!(
                "{P}reserved numbers 4 to 6 of t.v1.Kind are no longer reserved"
            )]
        );
        assert_compatible(&base, &with_enum("reserved 4 to 6, 9;"));
    }

    #[test]
    fn r12_switching_an_enum_between_open_and_closed_is_breaking() {
        let open = svc(&with_enum(""));
        let body2 = "service Svc { rpc Get(Req) returns (Rep); }\nmessage Rep {}\n\
                     message Req { optional Kind kind = 1; }\nenum Kind { KIND_UNSPECIFIED = 0; }\n";
        let closed = svc_with("proto2", body2);
        let messages = |old: &Contract, new: &Contract| -> Vec<String> {
            compare(old, Some(new))
                .into_iter()
                .map(|finding| finding.message)
                .collect()
        };
        assert_eq!(
            messages(&open, &closed),
            ["enum t.v1.Kind changed from open to closed"]
        );
        assert_eq!(
            messages(&closed, &open),
            ["enum t.v1.Kind changed from closed to open"]
        );
    }

    #[test]
    fn additions_renames_options_and_comments_are_compatible() {
        let base = with_req("string id = 1; oneof pick { string a = 2; }");
        let grown = "// A comment.\n\
                     service Svc {\n  rpc Get(Req) returns (Rep) { option deprecated = true; }\n  \
                     rpc More(New) returns (Rep);\n}\n\
                     message Rep { string extra = 1; }\n\
                     message New { Mood mood = 1; }\n\
                     enum Mood { MOOD_UNSPECIFIED = 0; }\n\
                     message Req {\n  string key = 1 [json_name = \"k\", deprecated = true];\n  \
                     oneof choice { string alt = 2; }\n  reserved 9;\n  reserved \"old\";\n}\n";
        assert_compatible(&base, grown);
    }

    #[test]
    fn types_no_longer_reachable_are_left_to_the_referencing_rule() {
        let base = format!(
            "{SERVICE}message Req {{ Inner inner = 1; }}\nmessage Inner {{ string x = 1; }}\n"
        );
        let changed = format!(
            "{SERVICE}message Req {{ Other inner = 1; }}\nmessage Other {{ int32 x = 1; }}\n\
             message Inner {{ bytes x = 1; }}\n"
        );
        let new = svc(&changed);
        assert!(!new.messages.contains_key("t.v1.Inner"));
        assert_eq!(
            breaking(&base, &changed),
            [format!(
                "{P}field 1 (inner) of t.v1.Req changed type from message:t.v1.Inner to \
                 message:t.v1.Other"
            )]
        );
    }

    #[test]
    fn findings_are_sorted_and_displayed_with_their_service() {
        let finding = Finding {
            service: "a.v1.A".into(),
            message: "m".into(),
        };
        assert_eq!(finding.to_string(), "breaking: a.v1.A: m");
        let base = with_req("string a = 1; string b = 2; string c = 10;");
        let findings = breaking(&base, &with_req(""));
        let mut sorted = findings.clone();
        sorted.sort();
        assert_eq!(findings, sorted);
        assert_eq!(findings.len(), 3);
        assert!(findings[0].contains("field 1 (a)"), "{findings:?}");
        assert!(findings[1].contains("field 10 (c)"), "{findings:?}");
    }

    #[test]
    fn uncovered_parts_of_a_range_are_computed_without_overflow() {
        assert_eq!(uncovered([1, 10], &[]), [[1, 10]]);
        assert_eq!(
            uncovered([1, 10], &[[0, 0], [3, 4], [8, 20]]),
            [[1, 2], [5, 7]]
        );
        assert!(uncovered([1, 10], &[[1, 10]]).is_empty());
        assert_eq!(uncovered([5, 6], &[[10, 12]]), [[5, 6]]);
        assert!(uncovered([5, i32::MAX], &[[1, i32::MAX]]).is_empty());
        assert_eq!(uncovered([5, i32::MAX], &[[1, 7], [9, i32::MAX]]), [[8, 8]]);
        assert!(covers(&[[1, 3]], 2));
        assert!(!covers(&[[1, 3]], 4));
    }
}
