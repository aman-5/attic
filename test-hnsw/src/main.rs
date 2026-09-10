use hnsw_rs::prelude::*;
struct A {
    hnsw: Hnsw<'static, f32, DistDot>,
}
fn main() {
    let a = A { hnsw: Hnsw::new(16, 1000, 16, 200, DistDot{}) };
    for _ in 0..10 {
        let v = vec![1.0, 0.0];
        a.hnsw.insert((&v, 1));
    }
}
