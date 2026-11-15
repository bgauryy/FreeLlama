#[cfg(test)]
mod metadata_limit_tests {
    use super::super::*;
    use crate::ollama_http::MAX_METADATA_BODY_BYTES;
    use axum::{Router, body::Body, routing::get};
    use std::convert::Infallible;

    struct Chunked(usize);

    impl http_body::Body for Chunked {
        type Data = axum::body::Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Ready(if self.0 == 0 {
                None
            } else {
                self.0 -= 1;
                Some(Ok(http_body::Frame::data(vec![b'x'; 1024 * 1024].into())))
            })
        }
    }

    #[tokio::test]
    async fn control_metadata_bounds_known_and_chunked_bodies_and_accepts_the_exact_limit() {
        let oversized = "x".repeat(MAX_METADATA_BODY_BYTES + 1);
        let exact = format!(
            "{{\"value\":\"{}\"}}",
            "x".repeat(MAX_METADATA_BODY_BYTES - 12)
        );
        assert_eq!(exact.len(), MAX_METADATA_BODY_BYTES);
        let server = Router::new()
            .route(
                "/known",
                get(move || {
                    let body = oversized.clone();
                    async move { body }
                }),
            )
            .route("/chunked", get(|| async { Body::new(Chunked(5)) }))
            .route(
                "/exact",
                get(move || {
                    let body = exact.clone();
                    async move { body }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let client = Client::new();
        for path in ["/known", "/chunked"] {
            let error = get_json(&client, &endpoint, path).await.unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
            assert!(
                error.body.error.contains("body exceeds metadata limit"),
                "{error:?}"
            );
        }
        assert_eq!(
            get_json(&client, &endpoint, "/exact").await.unwrap()["value"]
                .as_str()
                .unwrap()
                .len(),
            MAX_METADATA_BODY_BYTES - 12
        );
        worker.abort();
    }
}

#[cfg(test)]
mod resident_view_tests {
    use super::super::super::physical_placement_observation as observation_for;
    use super::super::*;

    fn view(placement: Backend, size: Option<u64>, vram: Option<u64>) -> Option<Value> {
        resident_view(
            placement,
            true,
            size,
            vram,
            &observation_for(placement, size, vram),
        )
    }

    #[test]
    fn full_gpu_full_cpu_and_mixed_splits() {
        assert_eq!(
            view(Backend::Gpu, Some(1000), Some(1000)).unwrap(),
            json!({"gpu_percent": 100, "processor": "gpu", "processor_label": "100% GPU", "assigned": false})
        );
        assert_eq!(
            view(Backend::Cpu, Some(1000), Some(0)).unwrap(),
            json!({"gpu_percent": 0, "processor": "cpu", "processor_label": "100% CPU", "assigned": true})
        );
        let mixed = resident_view(
            Backend::Gpu,
            true,
            Some(2000),
            Some(500),
            &json!({"processor": "mixed", "status": "verified"}),
        )
        .unwrap();
        assert_eq!(
            mixed,
            json!({
                "gpu_percent": 25,
                "processor": "mixed",
                "processor_label": "25% GPU / 75% CPU",
                "assigned": false,
                "warning": "Partially offloaded to CPU — expect a large slowdown. Free VRAM (`ollama_manage` action \"stop\") or lower the context length.",
            })
        );
    }

    #[test]
    fn observed_processor_overrides_the_byte_split() {
        let observed_gpu = resident_view(
            Backend::Gpu,
            true,
            Some(2000),
            Some(500),
            &json!({"processor": "gpu", "status": "verified"}),
        )
        .unwrap();
        assert_eq!(observed_gpu["gpu_percent"], 25);
        assert_eq!(observed_gpu["processor"], "gpu");
        assert_eq!(observed_gpu["processor_label"], "100% GPU");
        assert!(observed_gpu.get("warning").is_none());
        // An unclassified observation falls back to the byte split.
        let fallback = |vram| {
            resident_view(
                Backend::Gpu,
                true,
                Some(1000),
                Some(vram),
                &json!({"processor": "unknown"}),
            )
            .unwrap()["processor"]
                .clone()
        };
        assert_eq!(fallback(1000), "gpu");
        assert_eq!(fallback(0), "cpu");
        assert_eq!(fallback(400), "mixed");
    }

    #[test]
    fn mismatch_warns_about_the_configured_backend() {
        let mismatch = view(Backend::Cpu, Some(1000), Some(1000)).unwrap();
        assert_eq!(mismatch["assigned"], true);
        assert_eq!(mismatch["processor_label"], "100% GPU");
        assert_eq!(
            mismatch["warning"],
            "Configured cpu backend disagrees with Ollama /api/ps: gpu was physically observed. This sample is excluded from adaptive routing feedback."
        );
        let mixed_mismatch = view(Backend::Gpu, Some(2000), Some(500)).unwrap();
        assert_eq!(
            mixed_mismatch["warning"],
            "Configured gpu backend disagrees with Ollama /api/ps: mixed was physically observed. This sample is excluded from adaptive routing feedback."
        );
    }

    #[test]
    fn zero_missing_or_non_resident_sizes_pass_through() {
        assert!(view(Backend::Gpu, Some(0), Some(0)).is_none());
        assert!(view(Backend::Gpu, None, Some(500)).is_none());
        assert!(view(Backend::Gpu, Some(1000), None).is_none());
        assert!(resident_view(Backend::Gpu, false, Some(1000), Some(1000), &json!({})).is_none());
    }

    #[test]
    fn percent_matches_javascript_float_rounding() {
        // 29/200*100 is 14.499999999999998 in IEEE doubles, so Math.round gives 14, not 15.
        assert_eq!(rounded_percent(29, 200), 14);
        assert_eq!(rounded_percent(1, 8), 13);
        assert_eq!(rounded_percent(500, 2000), 25);
        assert_eq!(rounded_percent(1000, 1000), 100);
        assert_eq!(rounded_percent(1, 1000), 0);
    }
}

#[cfg(test)]
mod kv_estimate_tests {
    use super::super::super::{PlatformConfig, build, memory_kv_preflight_with_memory};
    use super::super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[tokio::test]
    async fn process_settings_refresh_after_cache_expiry_and_preserve_fresh_cache() {
        let mut config =
            PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11439", None, None);
        config.cpu_upstream = Some("http://127.0.0.1:11438".into());
        config.cpu_models.insert("cpu-helper:latest".into());
        let (_, state) = build(&config).unwrap();
        let old = ollama_env::resolve(
            Some(&BTreeMap::from([(
                "OLLAMA_NUM_PARALLEL".into(),
                "99".into(),
            )])),
            |_| None,
            |_| None,
            "test".into(),
        );
        *state.ollama.write().await = old.clone();
        *state.cpu_ollama.as_ref().unwrap().write().await = old;
        state.refresh_ollama_settings().await;
        assert_eq!(
            state.ollama.read().await.num_parallel(),
            99,
            "fresh observations must be reused"
        );
        *state.ollama_refresh.lock().await =
            Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
        state.refresh_ollama_settings().await;
        assert_ne!(state.ollama.read().await.num_parallel(), 99);
        assert_ne!(
            state
                .cpu_ollama
                .as_ref()
                .unwrap()
                .read()
                .await
                .num_parallel(),
            99
        );
        assert!(state.ollama_refresh.lock().await.elapsed() < Duration::from_secs(5));
    }

    fn llama_shape() -> Value {
        json!({"model_info": {
            "general.architecture": "llama",
            "llama.block_count": 16,
            "llama.attention.head_count": 16,
            "llama.attention.head_count_kv": 8,
            "llama.embedding_length": 1536,
        }})
    }

    #[test]
    fn kv_estimate_respects_explicit_key_and_value_dimensions() {
        let mut show = llama_shape();
        show["model_info"]["llama.attention.key_length"] = json!(128);
        show["model_info"]["llama.attention.value_length"] = json!(64);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]["llama.attention.value_length"] = json!(128);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
        // Explicit dimensions do not require a divisible embedding width.
        show["model_info"]["llama.embedding_length"] = json!(1537);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
    }

    #[test]
    fn kv_estimate_never_borrows_another_architectures_fields() {
        let mut show = llama_shape();
        show["model_info"]["clip.block_count"] = json!(999);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]
            .as_object_mut()
            .unwrap()
            .remove("llama.block_count");
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None);
    }

    #[test]
    fn kv_estimate_declares_nonuniform_or_invalid_shapes_unknown() {
        for (key, value) in [
            ("llama.attention.sliding_window", json!(512)),
            ("llama.attention.shared_kv_layers", json!(18)),
            ("llama.ssm.state_size", json!(16)),
            ("llama.attention.head_count_kv", json!([8, 0])),
            ("llama.attention.key_length", json!("128")),
            ("llama.attention.key_length", json!(0)),
            ("llama.block_count", json!(0)),
            ("llama.block_count", json!(u64::MAX)),
            ("general.architecture", json!("new_unknown_family")),
        ] {
            let mut show = llama_shape();
            show["model_info"][key] = value;
            assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None, "{key}");
        }
    }

    #[test]
    fn kv_preflight_does_not_call_f16_a_known_runtime_floor() {
        let model = CatalogModel {
            digest: None,
            name: "test".into(),
            size: 600,
            capabilities: BTreeSet::new(),
            other_capabilities: Vec::new(),
            advertised_context: None,
            kv_cache_bytes_per_token_f16: Some(10),
            modelfile_num_ctx: None,
            architecture: None,
            thinking: None,
            resident: false,
            resident_size: None,
            resident_vram: None,
            resident_context: None,
            benchmark: BTreeMap::new(),
            observed_loops: BTreeMap::new(),
            learned_output_tokens: None,
            bytes_per_token_milli: None,
            bytes_per_token_samples: 0,
            vision_tokens: None,
            policy_rank: BTreeMap::new(),
            policy_options: BTreeMap::new(),
        };
        let report = memory_kv_preflight_with_memory(&model, Some(30), true, Some(1000));
        assert_eq!(report["refuses"], false);
        assert_eq!(report["status"], "f16_estimate_exceeds_host_budget");
        assert_eq!(report["model_plus_kv_bytes_f16_estimate"], 900);
        assert!(report.get("known_runtime_floor_bytes").is_none());
        let mut large = model.clone();
        large.size = 900;
        large.kv_cache_bytes_per_token_f16 = None;
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), true, Some(1000))["refuses"],
            true
        );
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), false, Some(1000))["refuses"],
            false
        );
    }
}
