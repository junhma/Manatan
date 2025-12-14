use std::io::Cursor;

use anyhow::anyhow;
use chrome_lens_ocr::{LensClient, LensResult};
use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, ImageReader};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::merge::{self, MergeConfig};

// --- GraphQL Query Definitions ---

const MANGA_CHAPTERS_QUERY: &str = r#"
query MangaIdToChapterIDs($id: Int!) {
  manga(id: $id) {
    chapters {
      nodes {
        id
        chapterNumber
      }
    }
  }
}
"#;

const GET_CHAPTER_PAGES_QUERY: &str = r#"
mutation GET_CHAPTER_PAGES_FETCH($input: FetchChapterPagesInput!) {
  fetchChapterPages(input: $input) {
    chapter {
      id
      pageCount
    }
  }
}
"#;

// --- GraphQL Structs ---

#[derive(Deserialize)]
struct ChapterPageCountResponse {
    data: Option<ChapterPageCountData>,
}

#[derive(Deserialize)]
struct ChapterPageCountData {
    manga: Option<MangaChaptersNode>,
}

#[derive(Deserialize)]
struct MangaChaptersNode {
    chapters: Option<ChapterList>,
}

#[derive(Deserialize)]
struct ChapterList {
    nodes: Option<Vec<ChapterNode>>,
}

#[derive(Deserialize)]
struct ChapterNode {
    id: i32,
    #[serde(rename = "chapterNumber")]
    chapter_number: f64,
}

#[derive(Deserialize)]
struct FetchPagesResponse {
    data: Option<FetchPagesData>,
}

#[derive(Deserialize)]
struct FetchPagesData {
    #[serde(rename = "fetchChapterPages")]
    fetch_chapter_pages: Option<FetchChapterPagesNode>,
}

#[derive(Deserialize)]
struct FetchChapterPagesNode {
    chapter: Option<FetchedChapterNode>,
}

#[derive(Deserialize)]
struct FetchedChapterNode {
    #[serde(rename = "pageCount")]
    page_count: Option<usize>,
}

async fn execute_graphql_request(
    query_body: serde_json::Value,
    user: Option<String>,
    pass: Option<String>,
) -> anyhow::Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let graphql_url = "http://127.0.0.1:4568/api/graphql";

    let mut req = client
        .post(graphql_url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .json(&query_body);

    if let Some(u) = user {
        req = req.basic_auth(u, pass);
    }

    let resp = req.send().await?;
    let status = resp.status();

    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "[Failed to read body]".to_string());
        return Err(anyhow!(
            "GraphQL request failed (Status: {}). Body: {}",
            status,
            body
        ));
    }

    Ok(resp)
}

pub async fn resolve_total_pages_from_graphql(
    chapter_base_url: &str,
    user: Option<String>,
    pass: Option<String>,
) -> anyhow::Result<usize> {
    let path = get_cache_key(chapter_base_url);

    let parts: Vec<&str> = path.split('/').collect();

    let manga_id_str = parts
        .iter()
        .find(|&p| *p == "manga")
        .and_then(|_| parts.get(parts.iter().position(|&p| p == "manga")? + 1))
        .ok_or_else(|| anyhow!("Failed to parse manga ID from URL: {}", chapter_base_url))?;

    let chapter_number_str = parts
        .iter()
        .find(|&p| *p == "chapter")
        .and_then(|_| parts.get(parts.iter().position(|&p| p == "chapter")? + 1))
        .ok_or_else(|| {
            anyhow!(
                "Failed to parse chapter number from URL: {}",
                chapter_base_url
            )
        })?;

    let manga_id = manga_id_str.parse::<i32>()?;
    let chapter_number = chapter_number_str.parse::<f64>()?;

    let query_body = serde_json::json!({
        "operationName": "MangaIdToChapterIDs",
        "variables": { "id": manga_id },
        "query": MANGA_CHAPTERS_QUERY,
    });

    let resp = execute_graphql_request(query_body, user.clone(), pass.clone()).await?;

    let json: ChapterPageCountResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("Error decoding STEP 1 GraphQL response: {}", e))?;

    let chapters: Vec<ChapterNode> = json
        .data
        .and_then(|d| d.manga)
        .and_then(|m| m.chapters)
        .and_then(|c| c.nodes)
        .ok_or_else(|| anyhow!("GraphQL STEP 1 response missing chapter nodes"))?;

    let has_chapter_zero = chapters.iter().any(|ch| ch.chapter_number == 0.0);
    let target_chapter_num = if has_chapter_zero {
        chapter_number - 1.0
    } else {
        chapter_number
    };

    let matching_chapter = chapters
        .into_iter()
        .find(|ch| (ch.chapter_number - target_chapter_num).abs() < 0.001);

    let internal_chapter_id = matching_chapter.map(|ch| ch.id).ok_or_else(|| {
        anyhow!(
            "Failed to find internal ID for chapter number {}",
            target_chapter_num
        )
    })?;

    let mutation_body = serde_json::json!({
        "operationName": "GET_CHAPTER_PAGES_FETCH",
        "variables": {
            "input": { "chapterId": internal_chapter_id }
        },
        "query": GET_CHAPTER_PAGES_QUERY,
    });

    let resp = execute_graphql_request(mutation_body, user, pass).await?;

    let json: FetchPagesResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("Error decoding STEP 2 GraphQL response: {}", e))?;

    let page_count = json
        .data
        .and_then(|d| d.fetch_chapter_pages)
        .and_then(|f| f.chapter)
        .and_then(|c| c.page_count)
        .ok_or_else(|| anyhow!("GraphQL STEP 2 response missing page count"))?;

    Ok(page_count)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OcrResult {
    pub text: String,

    #[serde(rename = "tightBoundingBox")]
    pub tight_bounding_box: BoundingBox,

    #[serde(rename = "isMerged", skip_serializing_if = "Option::is_none")]
    pub is_merged: Option<bool>,

    #[serde(rename = "forcedOrientation", skip_serializing_if = "Option::is_none")]
    pub forced_orientation: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Helper to strip the scheme/host/query from the URL for caching purposes.
pub fn get_cache_key(url: &str) -> String {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        return parsed.path().to_string();
    }
    url.split('?').next().unwrap_or(url).to_string()
}

fn decode_avif_custom(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    let mut reader = Cursor::new(bytes);

    let decoder = avif_decode::Decoder::from_reader(&mut reader)
        .map_err(|e| anyhow!("avif-decode failed to parse: {e:?}"))?;

    let image = decoder
        .to_image()
        .map_err(|e| anyhow!("avif-decode failed to decode: {e:?}"))?;

    match image {
        avif_decode::Image::Rgb8(img) => {
            let raw_data: Vec<u8> = img.buf().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            let buffer = ImageBuffer::from_raw(img.width() as u32, img.height() as u32, raw_data)
                .ok_or_else(|| anyhow!("Failed to create RGB8 buffer"))?;
            Ok(DynamicImage::ImageRgb8(buffer))
        }
        avif_decode::Image::Rgba8(img) => {
            let raw_data: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| [p.r, p.g, p.b, p.a])
                .collect();
            let buffer = ImageBuffer::from_raw(img.width() as u32, img.height() as u32, raw_data)
                .ok_or_else(|| anyhow!("Failed to create RGBA8 buffer"))?;
            Ok(DynamicImage::ImageRgba8(buffer))
        }
        avif_decode::Image::Rgb16(img) => {
            let raw_data: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| [(p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8])
                .collect();
            let buffer = ImageBuffer::from_raw(img.width() as u32, img.height() as u32, raw_data)
                .ok_or_else(|| anyhow!("Failed to create RGB8 buffer from 16-bit"))?;
            Ok(DynamicImage::ImageRgb8(buffer))
        }
        avif_decode::Image::Rgba16(img) => {
            let raw_data: Vec<u8> = img
                .buf()
                .iter()
                .flat_map(|p| {
                    [
                        (p.r >> 8) as u8,
                        (p.g >> 8) as u8,
                        (p.b >> 8) as u8,
                        (p.a >> 8) as u8,
                    ]
                })
                .collect();
            let buffer = ImageBuffer::from_raw(img.width() as u32, img.height() as u32, raw_data)
                .ok_or_else(|| anyhow!("Failed to create RGBA8 buffer from 16-bit"))?;
            Ok(DynamicImage::ImageRgba8(buffer))
        }
        _ => Err(anyhow!("Unsupported AVIF color type")),
    }
}

pub async fn fetch_and_process(
    url: &str,
    user: Option<String>,
    pass: Option<String>,
) -> anyhow::Result<Vec<OcrResult>> {
    // 0. Force URL to Localhost
    let target_url = match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_scheme("http");
            let _ = parsed.set_host(Some("127.0.0.1"));
            let _ = parsed.set_port(Some(4567));
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    };

    // 1. Fetch
    let client = reqwest::Client::new();
    let mut req = client.get(&target_url);
    if let Some(u) = user {
        req = req.basic_auth(u, pass);
    }
    let resp = req
        .send()
        .await?
        .error_for_status()
        .map_err(|err| anyhow!("Failed error_for_status (URL: {}): {err:?}", target_url))?;
    let bytes = resp.bytes().await?.to_vec();

    // 2. Decode Image (With AVIF Fix)
    // We guess the format first. If it is AVIF, we use our custom function.
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|err| anyhow!("Failed with_guessed_format: {err:?}"))?;

    let img = if reader.format() == Some(ImageFormat::Avif) {
        decode_avif_custom(&bytes)?
    } else {
        reader
            .decode()
            .map_err(|err| anyhow!("Failed decode: {err:?}"))?
    };

    let full_w = img.width();
    let full_h = img.height();
    let chunk_h_limit = 3000;

    let mut final_results = Vec::new();
    let lens_client = LensClient::new(None);

    // 3. Chunking Loop
    let mut y_curr = 0;
    while y_curr < full_h {
        let h_curr = std::cmp::min(chunk_h_limit, full_h - y_curr);
        if h_curr == 0 {
            break;
        }

        let chunk_img = img.view(0, y_curr, full_w, h_curr).to_image();
        let mut buf = Cursor::new(Vec::new());
        chunk_img
            .write_to(&mut buf, ImageFormat::Png)
            .map_err(|err| anyhow!("Failed write_to: {err:?}"))?;
        let chunk_bytes = buf.into_inner();

        // 4. Call Lens
        let lens_res = lens_client
            .process_image_bytes(&chunk_bytes, Some("jp"))
            .await
            .map_err(|err| anyhow!("Failed process_image_bytes: {err:?}"))?;

        // 5. Flatten LensResult & Convert Normalized Coords to Chunk Pixels
        let mut flat_lines = Vec::new();
        for para in lens_res.paragraphs {
            for line in para.lines {
                if let Some(geom) = line.geometry {
                    // Lens returns normalized coords (0.0 - 1.0) relative to the chunk.
                    // We must convert them to pixels for the auto_merge logic to work.

                    let norm_x = (geom.center_x - geom.width / 2.0) as f64;
                    let norm_y = (geom.center_y - geom.height / 2.0) as f64;
                    let norm_w = geom.width as f64;
                    let norm_h = geom.height as f64;

                    // Convert to Chunk Pixels
                    let px_x = norm_x * full_w as f64;
                    let px_y = norm_y * h_curr as f64;
                    let px_w = norm_w * full_w as f64;
                    let px_h = norm_h * h_curr as f64;

                    // Logic from JS `_groupOcrData`: isVertical = width <= height
                    let is_vertical = px_w <= px_h;
                    let orientation = if is_vertical {
                        "vertical"
                    } else {
                        "horizontal"
                    }
                    .to_string();

                    flat_lines.push(OcrResult {
                        text: line.text,
                        is_merged: Some(false),
                        forced_orientation: Some(orientation),
                        tight_bounding_box: BoundingBox {
                            x: px_x,
                            y: px_y,
                            width: px_w,
                            height: px_h,
                        },
                    });
                }
            }
        }

        // 6. Auto Merge (Operates on Pixels)
        let merged = merge::auto_merge(flat_lines, full_w, h_curr, &MergeConfig::default());

        // 7. Adjust Coordinates: Chunk Pixels -> Global Pixels -> Global Normalized
        for mut res in merged {
            // 1. Get Chunk Pixels (Result from merge is still in pixels relative to chunk)
            let x_chunk_px = res.tight_bounding_box.x;
            let y_chunk_px = res.tight_bounding_box.y;
            let w_chunk_px = res.tight_bounding_box.width;
            let h_chunk_px = res.tight_bounding_box.height;

            // 2. Convert to Global Pixels
            let y_global_px = y_chunk_px + (y_curr as f64);

            // 3. Normalize to Global Image (0.0 - 1.0)
            res.tight_bounding_box.x = x_chunk_px / full_w as f64;
            res.tight_bounding_box.width = w_chunk_px / full_w as f64;
            res.tight_bounding_box.y = y_global_px / full_h as f64;
            res.tight_bounding_box.height = h_chunk_px / full_h as f64;

            final_results.push(res);
        }

        y_curr += chunk_h_limit;
    }

    Ok(final_results)
}
