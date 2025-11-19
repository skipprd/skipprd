use futures::StreamExt;

pub async fn buffer_unordered_map<I, Fut, T>(
	inputs: Vec<I>,
	concurrency: usize,
	mut f: impl FnMut(I) -> Fut,
) -> Vec<T>
where
	Fut: std::future::Future<Output = T>,
{
	futures::stream::iter(inputs.into_iter().map(|i| f(i)))
		.buffer_unordered(concurrency)
		.collect::<Vec<T>>()
		.await
}

pub fn dedup_pairs(pairs: &mut Vec<(String, String)>) {
	pairs.sort_by(|a,b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
	pairs.dedup();
}


