//! 集合工具（上游 `shared/collections.ts` 1:1 移植）。纯函数。

#![forbid(unsafe_code)]

/// 去重保序：按首次出现顺序返回去重后的数组（= JS `Array.from(new Set(items))`）。
/// 上游 `dedupe`。
pub fn dedupe<T: Clone + Eq + std::hash::Hash>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// 字符串去重 + 修剪空白 + 丢弃空串（dedupe 的 trim + filter(Boolean) 变体），保序。
/// 上游 `dedupeTrim`。
pub fn dedupe_trim(list: impl IntoIterator<Item = String>) -> Vec<String> {
    let trimmed: Vec<String> = list
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    dedupe(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_preserves_order() {
        assert_eq!(dedupe(vec![3, 1, 2, 1, 3, 4]), vec![3, 1, 2, 4]);
    }

    #[test]
    fn dedupe_strings() {
        assert_eq!(
            dedupe(vec!["a".to_string(), "b".to_string(), "a".to_string()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn dedupe_trim_filters_empty() {
        assert_eq!(
            dedupe_trim(vec![
                "  a  ".into(),
                "b".into(),
                "".into(),
                "  ".into(),
                "a".into()
            ]),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
