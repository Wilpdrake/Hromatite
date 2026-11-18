//! Selection of a concrete IL2CPP `LateUpdate` method.
//!
//! `HROMATITE_LATE_UPDATE=auto` accepts only `AUTO_CLASS`; `#index` and a raw
//! class address remain explicit manual overrides. Failure never selects QPC.
//! Being pre-original for the selected method does not reorder Unity's other
//! scripts, and installation alone is not liveness: `late` must report hits > 0.

#![forbid(unsafe_code)]

use crate::{log, m3};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selector {
    /// One fully-qualified class; arbitrary `LateUpdate` candidates are never a fallback.
    Auto,
    Index(usize),
    Address(usize),
}

/// Chosen from the live candidate list for the supported GameAssembly build
/// (see [`crate::gamebuild::SUPPORTED_SHA256`]).
/// Its index is only a materialisation hint; resolution always re-checks the name.
pub const AUTO_CLASS: &str = "VRC.Udon.UdonManager";
pub const AUTO_TYPE_INDEX: usize = 34_575;
pub const AUTO_BUILD_SHA256: &str = crate::gamebuild::SUPPORTED_SHA256;
const AUTO_SCAN_LIMIT: usize = 120_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub index: Option<usize>,
    pub class: usize,
    pub info: usize,
    pub entry: usize,
}

pub fn parse_selector(text: &str) -> Option<Selector> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Some(Selector::Auto);
    }
    let (index, radix, digits) = match text.strip_prefix('#') {
        Some(rest) => (true, 10, rest),
        None if text.starts_with("0x") || text.starts_with("0X") => (false, 16, &text[2..]),
        None if text.bytes().all(|b| b.is_ascii_digit()) => (false, 10, text),
        None => (false, 16, text),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = usize::from_str_radix(digits, radix).ok()?;
    if value == 0 {
        return None;
    }
    Some(if index {
        Selector::Index(value)
    } else {
        Selector::Address(value)
    })
}

pub fn candidate_from_methods(
    index: usize,
    class: usize,
    methods: &[m3::Method],
) -> Option<Candidate> {
    methods
        .iter()
        .find(|m| {
            m.name == "LateUpdate"
                && m.return_kind == 1 // IL2CPP_TYPE_VOID
                && m.parameters_count == 0
                && m.flags & 0x10 == 0 // not METHOD_ATTRIBUTE_STATIC
        })
        .map(|m| Candidate {
            index: Some(index),
            class,
            info: m.info,
            entry: m.entry,
        })
}

fn resolve_with<A, M>(selector: Selector, at: A, methods: M) -> Option<Candidate>
where
    A: FnOnce(usize) -> Option<usize>,
    M: FnOnce(usize) -> Vec<m3::Method>,
{
    let (index, key) = match selector {
        Selector::Index(i) => (Some(i), i),
        Selector::Address(a) => (None, a),
        Selector::Auto => return None,
    };
    let class = at(key)?;
    let mut found = candidate_from_methods(index.unwrap_or(0), class, &methods(class))?;
    found.index = index;
    Some(found)
}

pub fn auto_candidate_from<I, M, N>(classes: I, methods: M, full_name: N) -> Option<Candidate>
where
    I: IntoIterator<Item = (usize, usize)>,
    M: Fn(usize) -> Vec<m3::Method>,
    N: Fn(usize) -> String,
{
    let mut found = classes
        .into_iter()
        .filter(|(_, class)| full_name(*class) == AUTO_CLASS)
        .filter_map(|(index, class)| candidate_from_methods(index, class, &methods(class)));
    let candidate = found.next()?;
    // Two same-named implementations are ambiguous: guessing would make the
    // tick source depend on table order rather than evidence.
    if found.next().is_some() {
        return None;
    }
    Some(candidate)
}

pub fn resolve(selector: Selector) -> Option<Candidate> {
    match selector {
        Selector::Auto => auto_candidate_from(
            m3::classes(AUTO_SCAN_LIMIT),
            m3::methods,
            m3::class_full_name,
        ),
        Selector::Index(index) => resolve_with(selector, m3::class_at, m3::methods).or_else(|| {
            log::log_tag("late", &format!("class index {index} is not materialised"));
            None
        }),
        Selector::Address(_) => resolve_with(selector, Some, m3::methods),
    }
}

pub fn candidates_from<I, F>(classes: I, methods: F) -> Vec<Candidate>
where
    I: IntoIterator<Item = (usize, usize)>,
    F: Fn(usize) -> Vec<m3::Method>,
{
    classes
        .into_iter()
        .filter_map(|(index, class)| candidate_from_methods(index, class, &methods(class)))
        .collect()
}

pub fn candidates(max: usize) -> Vec<Candidate> {
    candidates_from(m3::classes(max), m3::methods)
}

/// Retry until `wait` has actually elapsed, including time spent in `attempt`.
/// The attempt is always made once, even for a zero-duration budget.
pub fn retry_for<T, F>(
    wait: std::time::Duration,
    delay: std::time::Duration,
    mut attempt: F,
) -> Option<T>
where
    F: FnMut(usize) -> Option<T>,
{
    let started = std::time::Instant::now();
    let mut attempt_index = 0usize;
    loop {
        if let Some(value) = attempt(attempt_index) {
            return Some(value);
        }
        let elapsed = started.elapsed();
        if elapsed >= wait {
            return None;
        }
        std::thread::sleep(delay.min(wait.saturating_sub(elapsed)));
        attempt_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_CLASS, Selector, auto_candidate_from, candidate_from_methods, candidates_from,
        parse_selector, resolve_with, retry_for,
    };
    use crate::m3::Method;

    fn method(name: &str, entry: usize) -> Method {
        Method {
            name: name.into(),
            entry,
            info: entry + 0x1000,
            return_kind: 1, // IL2CPP_TYPE_VOID
            parameters_count: 0,
            flags: 0,
        }
    }

    #[test]
    fn selector_distinguishes_type_indices_from_addresses() {
        assert_eq!(parse_selector("auto"), Some(Selector::Auto));
        assert_eq!(parse_selector(" AUTO "), Some(Selector::Auto));
        assert_eq!(parse_selector("#21033"), Some(Selector::Index(21033)));
        assert_eq!(
            parse_selector("0x30344d10"),
            Some(Selector::Address(0x30344d10))
        );
        assert_eq!(parse_selector("21033"), Some(Selector::Address(21033)));
        assert_eq!(parse_selector(""), None);
        assert_eq!(parse_selector("#"), None);
        assert_eq!(parse_selector("0x0"), None);
    }

    #[test]
    fn auto_resolver_requires_one_exact_named_valid_class() {
        let classes = vec![(1, 0x1000), (2, 0x2000), (3, 0x3000)];
        let methods = |class| vec![method("LateUpdate", class + 0x100)];
        let name = |class| match class {
            0x1000 => AUTO_CLASS.to_string(),
            0x2000 => "Other.VRCTweenPostLateUpdater".to_string(),
            _ => "VRC.SDK3.Components.Other".to_string(),
        };
        let found =
            auto_candidate_from(classes, methods, name).expect("exact auto class must resolve");
        assert_eq!(found.index, Some(1));
        assert_eq!(found.class, 0x1000);
        assert_eq!(found.entry, 0x1100);
    }

    #[test]
    fn auto_resolver_rejects_duplicates_and_never_picks_an_arbitrary_candidate() {
        let duplicate_classes = vec![(1, 0x1000), (2, 0x2000)];
        assert!(
            auto_candidate_from(
                duplicate_classes,
                |class| vec![method("LateUpdate", class + 0x100)],
                |_| AUTO_CLASS.to_string(),
            )
            .is_none()
        );
        assert!(
            auto_candidate_from(
                vec![(3, 0x3000)],
                |class| vec![method("LateUpdate", class + 0x100)],
                |_| "Other.Class".to_string(),
            )
            .is_none()
        );
        assert!(
            auto_candidate_from(
                vec![(1, 0x1000)],
                |_| vec![method("Update", 0x1100)],
                |_| AUTO_CLASS.to_string(),
            )
            .is_none()
        );
    }

    #[test]
    fn candidate_requires_an_exact_late_update_method() {
        let methods = [method("Update", 0x2000), method("LateUpdate", 0x3000)];
        let hit = candidate_from_methods(17, 0x1000, &methods).expect("LateUpdate must resolve");
        assert_eq!(hit.index, Some(17));
        assert_eq!(hit.class, 0x1000);
        assert_eq!(hit.info, 0x4000);
        assert_eq!(hit.entry, 0x3000);
        assert!(candidate_from_methods(18, 0x2000, &[method("lateupdate", 0x5000)]).is_none());
        assert!(candidate_from_methods(19, 0x2000, &[]).is_none());
    }

    #[test]
    fn candidate_rejects_unsupported_late_update_signatures() {
        for (return_kind, parameters_count, flags) in [(0x12, 0, 0), (1, 1, 0), (1, 0, 0x10)] {
            let unsupported = Method {
                name: "LateUpdate".into(),
                entry: 0x5000,
                info: 0x6000,
                return_kind,
                parameters_count,
                flags,
            };
            assert!(
                candidate_from_methods(4, 0x4444, std::slice::from_ref(&unsupported)).is_none()
            );
        }
    }

    #[test]
    fn resolver_uses_type_indices_and_preserves_address_only_selection() {
        let methods = |_class| vec![method("LateUpdate", 0x6000)];
        let by_index = resolve_with(Selector::Index(7), |i| (i == 7).then_some(0x7000), methods)
            .expect("index must resolve");
        assert_eq!(by_index.index, Some(7));
        assert_eq!(by_index.class, 0x7000);
        let by_address = resolve_with(Selector::Address(0x8000), |_| Some(0x8000), methods)
            .expect("address must resolve");
        assert_eq!(by_address.index, None);
        assert!(resolve_with(Selector::Index(9), |_| None, methods).is_none());
    }

    #[test]
    fn candidate_scan_skips_classes_without_late_update() {
        let found = candidates_from(
            vec![(1, 0x1000), (2, 0x2000), (3, 0x3000)],
            |class| match class {
                0x1000 | 0x3000 => vec![method("LateUpdate", class + 0x100)],
                _ => vec![method("Update", class + 0x100)],
            },
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].index, Some(1));
        assert_eq!(found[1].class, 0x3000);
    }

    #[test]
    fn wall_clock_deadline_includes_time_spent_inside_an_attempt() {
        let mut calls = 0;
        let result = retry_for(
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(1),
            |_| {
                calls += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
                None::<()>
            },
        );
        assert_eq!(result, None);
        assert_eq!(calls, 1, "a slow scan must not reset the retry budget");
    }

    #[test]
    fn retry_for_retries_until_ready_and_zero_budget_attempts_once() {
        let mut calls = 0;
        let ready = retry_for(
            std::time::Duration::from_millis(50),
            std::time::Duration::ZERO,
            |attempt| {
                calls += 1;
                (attempt == 2).then_some("ready")
            },
        );
        assert_eq!(ready, Some("ready"));
        assert_eq!(calls, 3);

        let mut failed_calls = 0;
        let expired = retry_for(std::time::Duration::ZERO, std::time::Duration::ZERO, |_| {
            failed_calls += 1;
            None::<()>
        });
        assert_eq!(expired, None);
        assert_eq!(failed_calls, 1);
    }
}
