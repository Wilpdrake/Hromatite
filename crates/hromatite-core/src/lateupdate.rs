//! Selection of a concrete IL2CPP `LateUpdate` method.

#![forbid(unsafe_code)]

use crate::{log, m3};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selector {
    Index(usize),
    Address(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub index: Option<usize>,
    pub class: usize,
    pub info: usize,
    pub entry: usize,
}

pub fn parse_selector(text: &str) -> Option<Selector> {
    let text = text.trim();
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
    Some(if index { Selector::Index(value) } else { Selector::Address(value) })
}

pub fn candidate_from_methods(index: usize, class: usize, methods: &[m3::Method]) -> Option<Candidate> {
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
    };
    let class = at(key)?;
    let mut found = candidate_from_methods(index.unwrap_or(0), class, &methods(class))?;
    found.index = index;
    Some(found)
}

pub fn resolve(selector: Selector) -> Option<Candidate> {
    match selector {
        Selector::Index(index) => resolve_with(selector, m3::class_at, m3::methods)
            .or_else(|| {
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

/// Call `attempt` at least once and at most `attempts` times. The caller decides
/// how to report errors; `None` means that the bounded wait expired.
pub fn retry_bounded<T, F>(attempts: u32, delay: std::time::Duration, mut attempt: F) -> Option<T>
where
    F: FnMut(usize) -> Option<T>,
{
    let attempts = attempts.max(1) as usize;
    for index in 0..attempts {
        if let Some(value) = attempt(index) {
            return Some(value);
        }
        if index + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{candidate_from_methods, candidates_from, parse_selector, resolve_with, retry_bounded, Selector};
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
        assert_eq!(parse_selector("#21033"), Some(Selector::Index(21033)));
        assert_eq!(parse_selector("0x30344d10"), Some(Selector::Address(0x30344d10)));
        assert_eq!(parse_selector("21033"), Some(Selector::Address(21033)));
        assert_eq!(parse_selector(""), None);
        assert_eq!(parse_selector("#"), None);
        assert_eq!(parse_selector("0x0"), None);
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
            assert!(candidate_from_methods(4, 0x4444, std::slice::from_ref(&unsupported)).is_none());
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
    fn bounded_retry_retries_until_ready_and_honours_the_limit() {
        let mut calls = 0;
        let ready = retry_bounded(4, std::time::Duration::ZERO, |attempt| {
            calls += 1;
            (attempt == 2).then_some("ready")
        });
        assert_eq!(ready, Some("ready"));
        assert_eq!(calls, 3);
        assert_eq!(retry_bounded(2, std::time::Duration::ZERO, |_| None::<()>), None);
    }
}
