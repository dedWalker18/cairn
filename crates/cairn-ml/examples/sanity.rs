//! cargo run --release -p cairn-ml --example sanity -- IMG...
//! Prints CLIP text/image similarities and bge similarities as a smoke test.
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let models = cairn_core::paths::models_dir()?;
    let t0 = std::time::Instant::now();
    let vision = cairn_ml::ClipVision::load(&models)?;
    let text = cairn_ml::ClipText::load(&models)?;
    let bge = cairn_ml::TextEmbedder::load(&models)?;
    eprintln!("load {:?}", t0.elapsed());
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let t0 = std::time::Instant::now();
    let px: Vec<Vec<u8>> = paths.iter().map(|p| cairn_core::extract::image_pixels(Path::new(p), "jpg")).collect::<Result<_, _>>()?;
    eprintln!("decode {:?}", t0.elapsed());
    let t0 = std::time::Instant::now();
    let iv = vision.embed(&px)?;
    eprintln!("vision {} imgs {:?}", px.len(), t0.elapsed());
    for q in ["a photo of mountains", "a photo of a cat", "a photo of a beach", "a photo of food", "a photo of a dog", "a photo of a computer"] {
        let t0 = std::time::Instant::now();
        let tv = text.embed(q)?;
        let el = t0.elapsed();
        let mut s: Vec<(f32, &str)> = iv.iter().zip(&paths).map(|(v, p)| (v.iter().zip(&tv).map(|(a, b)| a * b).sum(), p.rsplit('/').next().unwrap())).collect();
        s.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!("{q:28} ({el:?}) -> {:?}", &s[..3]);
    }
    let docs = vec!["#!/bin/bash\nterraform init\nterraform apply -auto-approve -var region=us-east-1".to_string(), "Quarterly sales figures for Q4 2025, revenue by region".to_string(), "Recipe for margherita pizza with basil".to_string()];
    let dv = bge.embed(&docs)?;
    for q in ["script that runs terraform on aws", "sales report last quarter", "cooking"] {
        let t0 = std::time::Instant::now();
        let qv = bge.embed_query(q)?;
        let el = t0.elapsed();
        let s: Vec<f32> = dv.iter().map(|d| d.iter().zip(&qv).map(|(a, b)| a * b).sum()).collect();
        println!("{q:36} ({el:?}) -> {s:?}");
    }
    Ok(())
}
