fn main() {
    println!("cargo:rerun-if-changed=res");
    embed_resource::compile("res/app.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
