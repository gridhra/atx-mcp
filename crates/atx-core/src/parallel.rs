//! 行単位の並列実行ヘルパ(`std::thread::scope` のみ、外部ランタイム非依存)。
//!
//! **決定論への影響はない**: 分割されるのは「画素間の実行順序」であって、
//! 1 画素内の累算順序(ops/mod.rs の決定論規約が禁じている再結合の対象)は
//! チャンク分割の有無に関わらず不変である。したがってスレッド数が変わっても
//! 出力バイト列は 1 バイトも変わらない。

/// 総数 `total` を CPU コア数に応じたチャンクへ分割する。
pub(crate) fn chunk_ranges(total: usize) -> Vec<std::ops::Range<usize>> {
    if total == 0 {
        return Vec::new();
    }
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(total)
        .max(1);
    let chunk = total.div_ceil(n_threads).max(1);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < total {
        let end = (start + chunk).min(total);
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// スライスを連続チャンクへ分割し、各チャンクを別スレッドで処理する。
pub(crate) fn for_each_chunk<T, F>(data: &mut [T], f: F)
where
    T: Send,
    F: Fn(&mut [T]) + Send + Sync,
{
    let total = data.len();
    let ranges = chunk_ranges(total);
    if ranges.len() <= 1 {
        f(data);
        return;
    }
    let f = &f;
    std::thread::scope(|scope| {
        let mut remaining = data;
        for range in ranges {
            let (chunk, rest) = remaining.split_at_mut(range.end - range.start);
            remaining = rest;
            scope.spawn(move || f(chunk));
        }
    });
}

/// 出力バッファを「1 行 = `row_len` 要素」のチャンクへ分けて並列に埋める。
///
/// クロージャは `(行番号, その行のスライス)` を受け取る。
pub(crate) fn fill_rows<T, F>(out: &mut [T], row_len: usize, rows: usize, f: F)
where
    T: Send,
    F: Fn(usize, &mut [T]) + Send + Sync,
{
    debug_assert_eq!(out.len(), row_len * rows);
    let ranges = chunk_ranges(rows);
    if ranges.len() <= 1 {
        for (y, row) in out.chunks_mut(row_len.max(1)).enumerate() {
            f(y, row);
        }
        return;
    }
    let f = &f;
    std::thread::scope(|scope| {
        let mut remaining = out;
        for range in ranges {
            let (chunk, rest) = remaining.split_at_mut((range.end - range.start) * row_len);
            remaining = rest;
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(row_len).enumerate() {
                    f(range.start + i, row);
                }
            });
        }
    });
}
