fn main() {
    let p = std::env::args().nth(1).expect("xlsx path");
    let bytes = std::fs::read(&p).expect("read");
    println!("{}", trilium_core::spreadsheet::xlsx_to_workbook(&bytes).expect("parse"));
}
