//! Differential fuzzing: the B+tree vs. Rust's `BTreeMap` as a trusted
//! oracle. Thousands of random inserts/deletes/gets/range-scans across
//! several seeds; any mismatch is a bug, and the failing seed replays it
//! deterministically.

use std::collections::BTreeMap;
use std::ops::Bound;

use glassdb::btree::BTree;
use glassdb::pager::Pager;
use glassdb::rng::Rng;
use glassdb::storage::MemStorage;
use glassdb::trace::new_shared;

fn fresh_pager() -> Pager {
    let mut storage = MemStorage::new();
    Pager::bootstrap(&mut storage, &BTree::empty_leaf_bytes()).unwrap();
    Pager::open(Box::new(storage), new_shared()).unwrap()
}

fn assert_full_scan_matches(
    tree: &BTree,
    pager: &mut Pager,
    model: &BTreeMap<i64, Vec<u8>>,
    seed: u64,
    op: usize,
) {
    let mut scanned: Vec<(i64, Vec<u8>)> = Vec::new();
    tree.scan(pager, Bound::Unbounded, Bound::Unbounded, &mut |k, v| {
        scanned.push((k, v.to_vec()));
        Ok(true)
    })
    .unwrap();
    let expected: Vec<(i64, Vec<u8>)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(
        scanned, expected,
        "full scan diverged from oracle (seed {seed}, op {op})"
    );
}

#[test]
fn btree_matches_btreemap_oracle() {
    for seed in 1..=8u64 {
        let mut pager = fresh_pager();
        let mut tree = BTree::create(&mut pager).unwrap();
        let mut model: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
        let mut rng = Rng::new(seed);

        for op in 0..4000 {
            // Small key space on purpose: collisions, replacements and
            // deletes of present keys must all get exercised.
            let key = rng.range_i64(0, 800);
            match rng.below(10) {
                0..=4 => {
                    let value = format!("value-{key}-op{op}")
                        .into_bytes()
                        .repeat(1 + rng.below(8) as usize);
                    let replaced = tree.insert(&mut pager, key, &value).unwrap();
                    let was_there = model.insert(key, value).is_some();
                    assert_eq!(replaced, was_there, "insert (seed {seed}, op {op})");
                }
                5..=6 => {
                    let removed = tree.delete(&mut pager, key).unwrap();
                    let was_there = model.remove(&key).is_some();
                    assert_eq!(removed, was_there, "delete (seed {seed}, op {op})");
                }
                7..=8 => {
                    let got = tree.get(&mut pager, key).unwrap();
                    assert_eq!(got, model.get(&key).cloned(), "get (seed {seed}, op {op})");
                }
                _ => {
                    let a = rng.range_i64(0, 800);
                    let b = rng.range_i64(0, 800);
                    let (lo, hi) = (a.min(b), a.max(b));
                    let mut scanned: Vec<(i64, Vec<u8>)> = Vec::new();
                    tree.scan(
                        &mut pager,
                        Bound::Included(lo),
                        Bound::Included(hi),
                        &mut |k, v| {
                            scanned.push((k, v.to_vec()));
                            Ok(true)
                        },
                    )
                    .unwrap();
                    let expected: Vec<(i64, Vec<u8>)> = model
                        .range((Bound::Included(lo), Bound::Included(hi)))
                        .map(|(k, v)| (*k, v.clone()))
                        .collect();
                    assert_eq!(
                        scanned, expected,
                        "range scan [{lo}, {hi}] diverged (seed {seed}, op {op})"
                    );
                }
            }
            if op % 500 == 0 {
                assert_full_scan_matches(&tree, &mut pager, &model, seed, op);
            }
        }
        assert_full_scan_matches(&tree, &mut pager, &model, seed, 4000);
        assert_eq!(
            tree.max_key(&mut pager).unwrap(),
            model.keys().next_back().copied(),
            "max_key (seed {seed})"
        );
        // Sanity: the workload was big enough to force real node splits.
        assert!(
            pager.header.page_count > 4,
            "tree never split — the fuzz isn't stressing anything (seed {seed})"
        );
    }
}

#[test]
fn sequential_inserts_then_ordered_scan() {
    let mut pager = fresh_pager();
    let mut tree = BTree::create(&mut pager).unwrap();
    for key in 0..10_000i64 {
        tree.insert(&mut pager, key, format!("row{key}").as_bytes())
            .unwrap();
    }
    let mut count = 0i64;
    tree.scan(
        &mut pager,
        Bound::Unbounded,
        Bound::Unbounded,
        &mut |k, v| {
            assert_eq!(k, count, "keys must come back in order");
            assert_eq!(v, format!("row{k}").as_bytes());
            count += 1;
            Ok(true)
        },
    )
    .unwrap();
    assert_eq!(count, 10_000);
    assert_eq!(tree.max_key(&mut pager).unwrap(), Some(9_999));
}

#[test]
fn values_at_the_size_cap_split_cleanly() {
    let mut pager = fresh_pager();
    let mut tree = BTree::create(&mut pager).unwrap();
    let big = vec![0xABu8; glassdb::btree::MAX_VALUE_LEN];
    for key in 0..50i64 {
        tree.insert(&mut pager, key, &big).unwrap();
    }
    for key in 0..50i64 {
        assert_eq!(
            tree.get(&mut pager, key).unwrap().as_deref(),
            Some(&big[..])
        );
    }
    // One byte over the cap is a clean error, not a corrupted page.
    let too_big = vec![0u8; glassdb::btree::MAX_VALUE_LEN + 1];
    assert!(tree.insert(&mut pager, 999, &too_big).is_err());
}
