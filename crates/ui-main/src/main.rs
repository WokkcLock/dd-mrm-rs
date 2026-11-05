//! dd-mrm —— slint 产品二进制（存档/模组管理器）。
//! 逻辑在 app.rs（UI 粘合）与 model.rs（纯逻辑，可测）。

mod app;
mod model;

fn main() {
    app::run();
}
