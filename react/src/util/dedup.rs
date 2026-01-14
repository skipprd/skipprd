pub fn dedup_pairs(pairs: &mut Vec<(String, String)>) {
    pairs.sort();
    pairs.dedup();
}

