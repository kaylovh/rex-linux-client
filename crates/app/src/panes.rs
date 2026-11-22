//! The terminal views of the blocks on screen, kept in step with the server's layout.

/// What to start and what to close to go from `current` blocks to `wanted` ones.
pub fn diff(current: &[String], wanted: &[String]) -> (Vec<String>, Vec<String>) {
    let mut add: Vec<String> = Vec::new();
    for w in wanted {
        if !current.contains(w) && !add.contains(w) {
            add.push(w.clone());
        }
    }
    let remove = current.iter().filter(|c| !wanted.contains(c)).cloned().collect();
    (add, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn diff_adds_and_removes() {
        let (add, remove) = diff(&v(&["a", "b"]), &v(&["b", "c"]));
        assert_eq!(add, v(&["c"]));
        assert_eq!(remove, v(&["a"]));
    }

    #[test]
    fn removed_block_is_closed() {
        let (add, remove) = diff(&v(&["a", "b", "c"]), &v(&["a", "c"]));
        assert!(add.is_empty());
        assert_eq!(remove, v(&["b"]));
    }

    #[test]
    fn reconcile_to_empty_closes_all() {
        let (add, remove) = diff(&v(&["a", "b"]), &[]);
        assert!(add.is_empty());
        assert_eq!(remove, v(&["a", "b"]));
    }

    #[test]
    fn duplicates_in_wanted_start_once() {
        let (add, _) = diff(&[], &v(&["a", "a"]));
        assert_eq!(add, v(&["a"]));
    }
}
