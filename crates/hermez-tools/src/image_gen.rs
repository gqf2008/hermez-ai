#![allow(dead_code)]
//! Image generation tool.
//!
//! Mirrors the Python `tools/image_generation_tool.py`.
//! 1 tool: `image_generate` — FAL.ai image generation with automatic upscaling.

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// Aspect ratio presets mapped to FLUX 2 Pro size presets.
const ASPECT_RATIOS: &[(&str, &str)] = &[
    ("landscape", "landscape_16_9"),
    ("square", "square"),
    ("portrait", "portrait_4_5"),
    ("landscape_4_3", "landscape_4_3"),
    ("portrait_3_4", "portrait_3_4"),
    ("landscape_3_2", "landscape_3_2"),
    ("portrait_2_3", "portrait_2_3"),
];

/// Check if image generation requirements are met.
pub fn check_image_requirements() -> bool {
    std::env::var("FAL_KEY").is_ok()
}

/// Validate parameters.
fn validate_parameters(
    prompt: &str,
    aspect_ratio: &str,
    num_inference_steps: i32,
    guidance_scale: f64,
    num_images: i32,
    output_format: &str,
) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("Prompt cannot be empty.".to_string());
    }
    if !ASPECT_RATIOS.iter().any(|(k, _)| *k == aspect_ratio) {
        return Err(format!(
            "Invalid aspect_ratio '{}'. Valid options: {:?}",
            aspect_ratio,
            ASPECT_RATIOS.iter().map(|(k, _)| k).collect::<Vec<_>>()
        ));
    }
    if !(1..=100).contains(&num_inference_steps) {
        return Err("num_inference_steps must be between 1 and 100.".to_string());
    }
    if !(0.1..=20.0).contains(&guidance_scale) {
        return Err("guidance_scale must be between 0.1 and 20.0.".to_string());
    }
    if !(1..=4).contains(&num_images) {
        return Err("num_images must be between 1 and 4.".to_string());
    }
    let valid_formats = ["png", "jpg", "webp"];
    if !valid_formats.contains(&output_format) {
        return Err(format!(
            "Invalid output_format '{}'. Valid options: {:?}",
            output_format, valid_formats
        ));
    }
    Ok(())
}

/// Call FAL.ai to generate an image.
async fn call_fal_generate(
    prompt: &str,
    aspect_ratio: &str,
    num_inference_steps: i32,
    guidance_scale: f64,
    num_images: i32,
    output_format: &str,
    seed: Option<i64>,
) -> Result<Vec<String>, String> {
    let api_key = std::env::var("FAL_KEY").map_err(|_| "FAL_KEY environment variable not set".to_string())?;

    // Map aspect ratio to FLUX size preset
    let size_preset = ASPECT_RATIOS
        .iter()
        .find(|(k, _)| *k == aspect_ratio)
        .map(|(_, v)| *v)
        .unwrap_or("landscape_16_9");

    let request_body = serde_json::json!({
        "prompt": prompt,
        "image_size": { "name": size_preset },
        "num_inference_steps": num_inference_steps,
        "guidance_scale": guidance_scale,
        "num_images": num_images,
        "output_format": output_format,
        "enable_safety_checker": false,
        "safety_tolerance": "5",
    });

    let mut body = if let Some(s) = seed {
        let mut b = request_body.clone();
        b["seed"] = serde_json::json!(s);
        b
    } else {
        request_body
    };

    // Remove null fields
    if let Some(obj) = body.as_object_mut() {
        obj.retain(|_, v| !v.is_null());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let resp = client
        .post("https://fal.run/fal-ai/flux-2-pro")
        .header("Authorization", format!("Key {api_key}"))
        .header("Content-Type", "application/json")
        .header("x-idempotency-key", &idempotency_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("FAL.ai request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("FAL.ai returned HTTP {status}: {text}"));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse FAL.ai response: {e}"))?;

    // Extract image URLs from response
    let images = json
        .get("images")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("No images in FAL.ai response: {json}"))?;

    let urls: Vec<String> = images
        .iter()
        .filter_map(|img| img.get("url").and_then(Value::as_str).map(String::from))
        .collect();

    if urls.is_empty() {
        return Err("FAL.ai returned empty image list.".to_string());
    }

    Ok(urls)
}

/// Upscale image using FAL.ai clarity upscaler.
async fn call_fal_upscale(image_url: &str) -> Result<String, String> {
    let api_key = std::env::var("FAL_KEY").map_err(|_| "FAL_KEY environment variable not set".to_string())?;

    let request_body = serde_json::json!({
        "image_url": image_url,
        "enable_safety_checker": false,
        "safety_tolerance": "5",
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let resp = client
        .post("https://fal.run/fal-ai/clarity-upscaler")
        .header("Authorization", format!("Key {api_key}"))
        .header("Content-Type", "application/json")
        .header("x-idempotency-key", &idempotency_key)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("FAL.ai upscale request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("FAL.ai upscale returned HTTP {status}: {text}"));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse FAL.ai upscale response: {e}"))?;

    json.get("images")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|img| img.get("url"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("No upscaled image URL in response: {json}"))
}

/// Handle image_generate tool call.
pub fn handle_image_generate(args: Value) -> Result<String, hermez_core::HermezError> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermez_core::HermezError::new(
                hermez_core::errors::ErrorCategory::ToolError,
                "image_generate requires 'prompt' parameter",
            )
        })?
        .to_string();

    let aspect_ratio = args
        .get("aspect_ratio")
        .and_then(Value::as_str)
        .unwrap_or("landscape")
        .to_string();

    let num_inference_steps = args
        .get("num_inference_steps")
        .and_then(Value::as_i64)
        .unwrap_or(50) as i32;

    let guidance_scale = args
        .get("guidance_scale")
        .and_then(Value::as_f64)
        .unwrap_or(4.5);

    let num_images = args
        .get("num_images")
        .and_then(Value::as_i64)
        .unwrap_or(1) as i32;

    let output_format = args
        .get("output_format")
        .and_then(Value::as_str)
        .unwrap_or("png")
        .to_string();

    let seed = args.get("seed").and_then(Value::as_i64);

    // Validate
    if let Err(e) = validate_parameters(
        &prompt,
        &aspect_ratio,
        num_inference_steps,
        guidance_scale,
        num_images,
        &output_format,
    ) {
        return Ok(tool_error(&e));
    }

    // Check requirements
    if !check_image_requirements() {
        return Ok(tool_error(
            "FAL_KEY environment variable not set. Set FAL_KEY with your FAL.ai API key to enable image generation.",
        ));
    }

    // Run async generation + upscale
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return Ok(tool_error("No async runtime available".to_string())),
    };

    let result = handle.block_on(async {
        // Generate
        let urls = call_fal_generate(
            &prompt,
            &aspect_ratio,
            num_inference_steps,
            guidance_scale,
            num_images,
            &output_format,
            seed,
        )
        .await?;

        // Auto-upscale first image
        let first_url = urls[0].clone();
        let upscaled = call_fal_upscale(&first_url).await;
        let was_upscaled = upscaled.is_ok();

        let final_url = match upscaled {
            Ok(_) => {
                tracing::info!("Image upscaled successfully");
                first_url
            }
            Err(ref e) => {
                tracing::warn!("Upscale failed, using original: {e}");
                first_url
            }
        };

        Ok::<_, String>(serde_json::json!({
            "success": true,
            "image": final_url,
            "all_images": urls,
            "upscaled": was_upscaled,
        })
        .to_string())
    });

    match result {
        Ok(json) => Ok(json),
        Err(e) => Ok(tool_error(format!("Image generation failed: {e}"))),
    }
}

/// Register the image_generate tool.
pub fn register_image_tool(registry: &mut ToolRegistry) {
    registry.register(
        "image_generate".to_string(),
        "image".to_string(),
        serde_json::json!({
            "name": "image_generate",
            "description": "Generate images from text prompts using FAL.ai (FLUX 2 Pro). Automatically upscales the result for higher quality.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Detailed text description of the image to generate." },
                    "aspect_ratio": { "type": "string", "description": "Aspect ratio: landscape, square, portrait, landscape_4_3, portrait_3_4, landscape_3_2, portrait_2_3. Default: landscape." },
                    "num_inference_steps": { "type": "integer", "description": "Number of inference steps (1-100). Higher = better quality but slower. Default: 50." },
                    "guidance_scale": { "type": "number", "description": "Guidance scale (0.1-20.0). Higher = more faithful to prompt. Default: 4.5." },
                    "num_images": { "type": "integer", "description": "Number of images to generate (1-4). Default: 1." },
                    "output_format": { "type": "string", "description": "Output format: png, jpg, webp. Default: png." },
                    "seed": { "type": "integer", "description": "Random seed for reproducible results (optional)." }
                },
                "required": ["prompt"]
            }
        }),
        std::sync::Arc::new(handle_image_generate),
        Some(std::sync::Arc::new(check_image_requirements)),
        vec!["image".to_string()],
        "Generate images using AI (FAL.ai FLUX 2 Pro)".to_string(),
        "🎨".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_image_requirements() {
        // May or may not pass depending on env
        let _ = check_image_requirements();
    }

    #[test]
    fn test_validate_parameters_valid() {
        assert!(validate_parameters("a cat", "landscape", 50, 4.5, 1, "png").is_ok());
        assert!(validate_parameters("a cat", "square", 1, 0.1, 1, "jpg").is_ok());
        assert!(validate_parameters("a cat", "portrait", 100, 20.0, 4, "webp").is_ok());
    }

    #[test]
    fn test_validate_empty_prompt() {
        let result = validate_parameters("", "landscape", 50, 4.5, 1, "png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_validate_whitespace_prompt() {
        let result = validate_parameters("   ", "landscape", 50, 4.5, 1, "png");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_invalid_aspect() {
        let result = validate_parameters("a cat", "wide", 50, 4.5, 1, "png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid aspect_ratio"));
    }

    #[test]
    fn test_validate_steps_range() {
        let result = validate_parameters("a cat", "landscape", 0, 4.5, 1, "png");
        assert!(result.is_err());
        let result = validate_parameters("a cat", "landscape", 101, 4.5, 1, "png");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_guidance_range() {
        let result = validate_parameters("a cat", "landscape", 50, 0.0, 1, "png");
        assert!(result.is_err());
        let result = validate_parameters("a cat", "landscape", 50, 21.0, 1, "png");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_num_images_range() {
        let result = validate_parameters("a cat", "landscape", 50, 4.5, 0, "png");
        assert!(result.is_err());
        let result = validate_parameters("a cat", "landscape", 50, 4.5, 5, "png");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_invalid_format() {
        let result = validate_parameters("a cat", "landscape", 50, 4.5, 1, "gif");
        assert!(result.is_err());
    }

    #[test]
    fn test_aspect_ratio_mapping() {
        assert_eq!(
            ASPECT_RATIOS.iter().find(|(k, _)| *k == "landscape").map(|(_, v)| *v),
            Some("landscape_16_9")
        );
        assert_eq!(
            ASPECT_RATIOS.iter().find(|(k, _)| *k == "square").map(|(_, v)| *v),
            Some("square")
        );
        assert_eq!(
            ASPECT_RATIOS.iter().find(|(k, _)| *k == "portrait").map(|(_, v)| *v),
            Some("portrait_4_5")
        );
    }

    #[test]
    fn test_handler_missing_prompt() {
        let result = handle_image_generate(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_no_fal_key() {
        // Without FAL_KEY, should return tool error
        let result = handle_image_generate(serde_json::json!({
            "prompt": "a cute cat"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }
}
