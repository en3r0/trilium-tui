use trilium_core::search::parse::{parse, Ctx, SearchOpts};
fn main() {
    let q = std::env::args().nth(1).unwrap();
    let mut ctx = Ctx::default();
    let p = parse(&q, &SearchOpts::default(), &mut ctx);
    println!("{:#?}\nerror: {:?}", p.expr, ctx.error);
}
