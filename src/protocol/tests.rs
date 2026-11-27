#[cfg(test)]
mod tests {
    use super::*;

    fn hook_event(kind: HookEventKind) -> HookEvent {
        HookEvent {
            kind,
            cwd: None,
            tool_name: None,
            message: None,
            notification_type: None,
            level: None,
        }
    }

    fn hook_request(provider_session_id: &str, event: HookEvent) -> RequestEnvelope {
        RequestEnvelope {
            id: "request-hook".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::HookEvent {
                worker: WorkerKind::Grok,
                provider_session_id: provider_session_id.to_owned(),
                event,
            },
        }
    }

    fn valid_provider_session_id() -> &'static str {
        "01234567-89ab-4def-8123-456789abcdef"
    }

    #[test]
    fn request_round_trip_preserves_create_fields() {
        let request = RequestEnvelope {
            id: "request-1".to_owned(),
            client_kind: Some(ClientKind::Agy),
            client_session_id: Some("agy:conv-42".to_owned()),
            request: Request::Create {
                worker: WorkerKind::Codex,
                cwd: r"C:\work tree\repo".to_owned(),
                prompt: Some("fix the terminal".to_owned()),
                model: Some("gpt-5-codex".to_owned()),
                owner: Some("agy-conv-42".to_owned()),
                always_approve: true,
                sandbox: Some(CodexSandboxMode::WorkspaceWrite),
                dangerously_bypass: false,
            },
        };

        let frame = encode_frame(&request).unwrap();
        assert_eq!(frame.last(), Some(&b'\n'));
        assert_eq!(decode_request(&frame).unwrap(), request);
    }

    #[test]
    fn rejects_codex_only_flags_for_grok_worker() {
        let with_sandbox = RequestEnvelope {
            id: "r1".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::Create {
                worker: WorkerKind::Grok,
                cwd: ".".to_owned(),
                prompt: None,
                model: None,
                owner: None,
                always_approve: false,
                sandbox: Some(CodexSandboxMode::WorkspaceWrite),
                dangerously_bypass: false,
            },
        };
        assert!(decode_request(&encode_frame(&with_sandbox).unwrap()).is_err());

        let with_bypass = RequestEnvelope {
            id: "r2".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::Create {
                worker: WorkerKind::Grok,
                cwd: ".".to_owned(),
                prompt: None,
                model: None,
                owner: None,
                always_approve: false,
                sandbox: None,
                dangerously_bypass: true,
            },
        };
        assert!(decode_request(&encode_frame(&with_bypass).unwrap()).is_err());
    }

    #[test]
    fn request_round_trip_preserves_hook_event_fields() {
        let request = hook_request(
            "01234567-89AB-4DEF-8123-456789ABCDEF",
            HookEvent {
                kind: HookEventKind::Notification,
                cwd: Some(r"C:\work tree\repo".to_owned()),
                tool_name: Some("ask_user_question".to_owned()),
                message: Some("需要用户确认".to_owned()),
                notification_type: Some("permission_prompt".to_owned()),
                level: Some("info".to_owned()),
            },
        );

        let frame = encode_frame(&request).unwrap();
        assert_eq!(decode_request(&frame).unwrap(), request);
    }

    #[test]
    fn decodes_every_request_kind() {
        let requests = [
            Request::ServerStatus,
            Request::ServerStop,
            Request::Heartbeat,
            Request::CloseClient,
            Request::List,
            Request::Show {
                session: "session-1".to_owned(),
            },
            Request::Read {
                session: "session-1".to_owned(),
                cursor: Some(12),
                limit: Some(200),
                wait_ms: Some(500),
            },
            Request::Send {
                session: "session-1".to_owned(),
                input: "\r".to_owned(),
            },
            Request::Write {
                session: "session-1".to_owned(),
                data_base64: BASE64.encode([0_u8, 0xff, b'\r']),
            },
            Request::Resize {
                session: "session-1".to_owned(),
                cols: 120,
                rows: 36,
            },
            Request::Wait {
                session: "session-1".to_owned(),
                for_condition: WaitCondition::TuiIdle,
                timeout_ms: Some(30_000),
            },
            Request::Close {
                session: "session-1".to_owned(),
            },
            Request::HookEvent {
                worker: WorkerKind::Codex,
                provider_session_id: valid_provider_session_id().to_owned(),
                event: hook_event(HookEventKind::Stop),
            },
        ];

        for (index, request) in requests.into_iter().enumerate() {
            let envelope = RequestEnvelope {
                id: format!("request-{index}"),
                client_kind: None,
                client_session_id: None,
                request,
            };
            assert_eq!(
                decode_request(&encode_frame(&envelope).unwrap()).unwrap(),
                envelope
            );
        }

        let legacy_close = br#"{"id":"r-legacy","request":{"method":"close_codex"}}"#;
        assert_eq!(
            decode_request(legacy_close).unwrap().request,
            Request::CloseClient
        );
    }

    #[test]
    fn accepts_one_optional_line_ending_and_rejects_multiple_frames() {
        let json = br#"{"id":"r1","request":{"method":"server_status"}}"#;
        assert!(decode_request(json).is_ok());

        let mut crlf = json.to_vec();
        crlf.extend_from_slice(b"\r\n");
        assert!(decode_request(&crlf).is_ok());

        let mut multiple = json.to_vec();
        multiple.push(b'\n');
        multiple.extend_from_slice(json);
        multiple.push(b'\n');
        assert!(decode_request(&multiple).is_err());
    }

    #[test]
    fn rejects_oversized_frame() {
        let frame = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(decode_request(&frame).is_err());
        assert!(encode_frame(&"x".repeat(MAX_FRAME_BYTES)).is_err());
    }

    #[test]
    fn rejects_invalid_request_ids_and_session_handles() {
        let invalid_id = br#"{"id":"bad id","request":{"method":"server_status"}}"#;
        assert!(decode_request(invalid_id).is_err());

        let invalid_handle =
            br#"{"id":"r1","request":{"method":"show","params":{"session":"../bad"}}}"#;
        assert!(decode_request(invalid_handle).is_err());
    }

    #[test]
    fn validates_optional_session_owner() {
        let request = |owner: Option<String>| RequestEnvelope {
            id: "request-1".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::Create {
                worker: WorkerKind::Grok,
                cwd: ".".to_owned(),
                prompt: None,
                model: None,
                owner,
                always_approve: false,
                sandbox: None,
                dangerously_bypass: false,
            },
        };

        for owner in [
            None,
            Some("codex-thread-42".to_owned()),
            Some("人工会话".to_owned()),
        ] {
            let envelope = request(owner);
            assert!(decode_request(&encode_frame(&envelope).unwrap()).is_ok());
        }
        for owner in [Some(" ".to_owned()), Some("bad\nowner".to_owned())] {
            let envelope = request(owner);
            assert!(decode_request(&encode_frame(&envelope).unwrap()).is_err());
        }
        let oversized = request(Some("x".repeat(MAX_OWNER_BYTES + 1)));
        assert!(decode_request(&encode_frame(&oversized).unwrap()).is_err());
    }

    #[test]
    fn validates_provider_session_id_as_canonical_uuid() {
        for provider_session_id in [
            valid_provider_session_id(),
            "01234567-89AB-4DEF-8123-456789ABCDEF",
        ] {
            let request =
                hook_request(provider_session_id, hook_event(HookEventKind::SessionStart));
            assert!(decode_request(&encode_frame(&request).unwrap()).is_ok());
        }

        for provider_session_id in [
            "0123456789ab-4def-8123-456789abcdef",
            "01234567-89ab4-def-8123-456789abcdef",
            "01234567-89ab-4def-8123-456789abcdeg",
            "{01234567-89ab-4def-8123-456789abcdef}",
            "01234567-89ab-4def-8123-456789abcde",
        ] {
            let request =
                hook_request(provider_session_id, hook_event(HookEventKind::SessionStart));
            assert!(decode_request(&encode_frame(&request).unwrap()).is_err());
        }
    }

    #[test]
    fn validates_hook_event_field_limits() {
        let maximum = HookEvent {
            kind: HookEventKind::PreToolUse,
            cwd: Some("c".repeat(MAX_HOOK_CWD_BYTES)),
            tool_name: Some("t".repeat(MAX_HOOK_SHORT_TEXT_BYTES)),
            message: Some("m".repeat(MAX_HOOK_MESSAGE_BYTES)),
            notification_type: Some("n".repeat(MAX_HOOK_SHORT_TEXT_BYTES)),
            level: Some("l".repeat(MAX_HOOK_SHORT_TEXT_BYTES)),
        };
        let request = hook_request(valid_provider_session_id(), maximum);
        assert!(decode_request(&encode_frame(&request).unwrap()).is_ok());

        let oversized = [
            HookEvent {
                cwd: Some("c".repeat(MAX_HOOK_CWD_BYTES + 1)),
                ..hook_event(HookEventKind::SessionStart)
            },
            HookEvent {
                tool_name: Some("t".repeat(MAX_HOOK_SHORT_TEXT_BYTES + 1)),
                ..hook_event(HookEventKind::PreToolUse)
            },
            HookEvent {
                message: Some("m".repeat(MAX_HOOK_MESSAGE_BYTES + 1)),
                ..hook_event(HookEventKind::Notification)
            },
            HookEvent {
                notification_type: Some("n".repeat(MAX_HOOK_SHORT_TEXT_BYTES + 1)),
                ..hook_event(HookEventKind::Notification)
            },
            HookEvent {
                level: Some("l".repeat(MAX_HOOK_SHORT_TEXT_BYTES + 1)),
                ..hook_event(HookEventKind::Notification)
            },
        ];
        for event in oversized {
            let request = hook_request(valid_provider_session_id(), event);
            assert!(decode_request(&encode_frame(&request).unwrap()).is_err());
        }
    }

    #[test]
    fn rejects_forbidden_hook_event_characters() {
        let invalid = [
            HookEvent {
                cwd: Some("bad\0cwd".to_owned()),
                ..hook_event(HookEventKind::SessionStart)
            },
            HookEvent {
                tool_name: Some("bad\rtool".to_owned()),
                ..hook_event(HookEventKind::PreToolUse)
            },
            HookEvent {
                message: Some("bad\u{7f}message".to_owned()),
                ..hook_event(HookEventKind::Notification)
            },
            HookEvent {
                notification_type: Some("bad\tnotification".to_owned()),
                ..hook_event(HookEventKind::Notification)
            },
            HookEvent {
                level: Some("bad\nlevel".to_owned()),
                ..hook_event(HookEventKind::Notification)
            },
        ];
        for event in invalid {
            let request = hook_request(valid_provider_session_id(), event);
            assert!(decode_request(&encode_frame(&request).unwrap()).is_err());
        }
    }

    #[test]
    fn validates_raw_terminal_writes() {
        let request = |data_base64: String| RequestEnvelope {
            id: "request-1".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::Write {
                session: "session-1".to_owned(),
                data_base64,
            },
        };

        let maximum = request(BASE64.encode(vec![0x5a; MAX_WRITE_BYTES]));
        assert!(decode_request(&encode_frame(&maximum).unwrap()).is_ok());

        let empty = request(String::new());
        assert!(decode_request(&encode_frame(&empty).unwrap()).is_err());

        let oversized = request(BASE64.encode(vec![0x5a; MAX_WRITE_BYTES + 1]));
        assert!(decode_request(&encode_frame(&oversized).unwrap()).is_err());

        let malformed = request("not-base64!".to_owned());
        assert!(decode_request(&encode_frame(&malformed).unwrap()).is_err());
    }

    #[test]
    fn validates_terminal_resize_bounds() {
        let request = |cols, rows| RequestEnvelope {
            id: "request-1".to_owned(),
            client_kind: None,
            client_session_id: None,
            request: Request::Resize {
                session: "session-1".to_owned(),
                cols,
                rows,
            },
        };

        for (cols, rows) in [(20, 5), (500, 200), (120, 36)] {
            let envelope = request(cols, rows);
            assert!(decode_request(&encode_frame(&envelope).unwrap()).is_ok());
        }
        for (cols, rows) in [(19, 36), (501, 36), (120, 4), (120, 201)] {
            let envelope = request(cols, rows);
            assert!(decode_request(&encode_frame(&envelope).unwrap()).is_err());
        }
    }

    #[test]
    fn response_round_trip_validates_session_and_read_cursor() {
        let response = ResponseEnvelope::success(
            "request-1",
            ResponseResult::Read(ReadResult {
                session: "session-1".to_owned(),
                cursor: 10,
                next_cursor: 13,
                data_base64: BASE64.encode(b"abc"),
                plain_text: Some("abc".to_owned()),
                screen: Some("screen snapshot".to_owned()),
                truncated: false,
                eof: false,
            }),
        );

        assert_eq!(
            decode_response(&encode_frame(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn read_cursor_is_a_byte_offset() {
        for next_cursor in [9, 10, 15] {
            let response = ResponseEnvelope::success(
                "request-1",
                ResponseResult::Read(ReadResult {
                    session: "session-1".to_owned(),
                    cursor: 10,
                    next_cursor,
                    data_base64: BASE64.encode(b"data"),
                    plain_text: None,
                    screen: None,
                    truncated: false,
                    eof: false,
                }),
            );
            assert!(decode_response(&encode_frame(&response).unwrap()).is_err());
        }
    }

    #[test]
    fn rejects_inconsistent_response_envelopes() {
        let missing_result = br#"{"id":"r1","ok":true}"#;
        assert!(decode_response(missing_result).is_err());

        let both = br#"{"id":"r1","ok":false,"result":{"type":"accepted","value":{"accepted":true}},"error":{"code":"failed","message":"no"}}"#;
        assert!(decode_response(both).is_err());

        let empty_error = ResponseEnvelope::failure("r1", "failed", " ");
        assert!(decode_response(&encode_frame(&empty_error).unwrap()).is_err());
    }

    #[test]
    fn serializes_all_session_phases_in_snake_case() {
        let phases = [
            (SessionPhase::Starting, "\"starting\""),
            (SessionPhase::Running, "\"running\""),
            (SessionPhase::Idle, "\"idle\""),
            (SessionPhase::Exited, "\"exited\""),
            (SessionPhase::Failed, "\"failed\""),
            (SessionPhase::Stopped, "\"stopped\""),
        ];
        for (phase, expected) in phases {
            assert_eq!(serde_json::to_string(&phase).unwrap(), expected);
        }
        assert_eq!(
            serde_json::to_string(&WaitCondition::TuiIdle).unwrap(),
            "\"tui_idle\""
        );
    }

    #[test]
    fn serializes_all_hook_event_kinds_in_snake_case() {
        let kinds = [
            (HookEventKind::SessionStart, "\"session_start\""),
            (HookEventKind::UserPromptSubmit, "\"user_prompt_submit\""),
            (HookEventKind::Stop, "\"stop\""),
            (HookEventKind::StopFailure, "\"stop_failure\""),
            (HookEventKind::SessionEnd, "\"session_end\""),
            (HookEventKind::PreToolUse, "\"pre_tool_use\""),
            (HookEventKind::PostToolUse, "\"post_tool_use\""),
            (
                HookEventKind::PostToolUseFailure,
                "\"post_tool_use_failure\"",
            ),
            (HookEventKind::PermissionDenied, "\"permission_denied\""),
            (HookEventKind::PermissionRequest, "\"permission_request\""),
            (HookEventKind::Interrupt, "\"interrupt\""),
            (HookEventKind::Notification, "\"notification\""),
            (HookEventKind::SubagentStart, "\"subagent_start\""),
            (HookEventKind::SubagentStop, "\"subagent_stop\""),
            (HookEventKind::PreCompact, "\"pre_compact\""),
            (HookEventKind::PostCompact, "\"post_compact\""),
        ];
        for (kind, expected) in kinds {
            assert_eq!(serde_json::to_string(&kind).unwrap(), expected);
        }

        let activities = [
            (HookActivity::Unknown, "\"unknown\""),
            (HookActivity::Working, "\"working\""),
            (HookActivity::Waiting, "\"waiting\""),
            (HookActivity::Done, "\"done\""),
        ];
        for (activity, expected) in activities {
            assert_eq!(serde_json::to_string(&activity).unwrap(), expected);
        }
    }

    #[test]
    fn validates_session_timestamps_and_wait_state() {
        let invalid_session = ResponseEnvelope::success(
            "r1",
            ResponseResult::Session(Box::new(SessionState {
                session: "session-1".to_owned(),
                worker: WorkerKind::Grok,
                client_kind: Some(ClientKind::Codex),
                owner: Some("codex-thread-42".to_owned()),
                client_session_id: Some("codex-client-42".to_owned()),
                client_state: ClientLeaseState::Connected,
                client_lease_ms: Some(120_000),
                orphan_grace_ms: Some(600_000),
                client_last_seen_at_ms: Some(20),
                orphaned_at_ms: None,
                auto_close_at_ms: None,
                phase: SessionPhase::Running,
                title: Some("Grok".to_owned()),
                cwd: ".".to_owned(),
                model: None,
                always_approve: false,
                sandbox: None,
                dangerously_bypass: false,
                process_id: Some(42),
                screen: Some("screen".to_owned()),
                rows: 36,
                cols: 120,
                screen_ansi_base64: BASE64.encode(b"screen"),
                last_cursor: 12,
                last_output_at_ms: Some(15),
                created_at_ms: 20,
                updated_at_ms: 10,
                exit_code: None,
                error: None,
                activity: HookActivity::Waiting,
                hook_event: Some(HookEventKind::PreToolUse),
                hook_at_ms: Some(10),
                tool_name: Some("ask_user_question".to_owned()),
                waiting_reason: Some("Waiting for an answer".to_owned()),
            })),
        );
        assert!(decode_response(&encode_frame(&invalid_session).unwrap()).is_err());

        let invalid_wait = ResponseEnvelope::success(
            "r2",
            ResponseResult::Wait(WaitResult {
                session: "session-1".to_owned(),
                condition: WaitCondition::TuiIdle,
                satisfied: true,
                timed_out: true,
                phase: SessionPhase::Idle,
                exit_code: None,
                blocked_reason: None,
            }),
        );
        assert!(decode_response(&encode_frame(&invalid_wait).unwrap()).is_err());
    }

    #[test]
    fn serializes_web_events_message_wire_contract() {
        let session = SessionState {
            session: "gbt-1".to_owned(),
            worker: WorkerKind::Codex,
            client_kind: Some(ClientKind::Agy),
            owner: None,
            client_session_id: Some("agy:conv-1".to_owned()),
            client_state: ClientLeaseState::Connected,
            client_lease_ms: Some(120_000),
            orphan_grace_ms: Some(600_000),
            client_last_seen_at_ms: Some(1),
            orphaned_at_ms: None,
            auto_close_at_ms: None,
            phase: SessionPhase::Idle,
            title: None,
            cwd: "/tmp".to_owned(),
            model: None,
            always_approve: false,
            sandbox: Some(CodexSandboxMode::WorkspaceWrite),
            dangerously_bypass: false,
            process_id: None,
            screen: Some("hi".to_owned()),
            rows: 36,
            cols: 120,
            screen_ansi_base64: BASE64.encode(b"ansi"),
            last_cursor: 7,
            last_output_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            exit_code: None,
            error: None,
            activity: HookActivity::Unknown,
            hook_event: None,
            hook_at_ms: None,
            tool_name: None,
            waiting_reason: None,
        };
        let reset = TerminalStreamEntry::reset_snapshot(&session);
        assert!(reset.reset);
        assert_eq!(reset.cursor, 0);
        assert_eq!(reset.next_cursor, 7);
        assert_eq!(reset.data_base64, session.screen_ansi_base64);

        let delta = TerminalStreamEntry::delta(&ReadResult {
            session: "gbt-1".to_owned(),
            cursor: 3,
            next_cursor: 5,
            data_base64: BASE64.encode(b"xy"),
            plain_text: None,
            screen: None,
            truncated: false,
            eof: false,
        });
        assert!(!delta.reset);
        assert_eq!(delta.cursor, 3);
        assert_eq!(delta.next_cursor, 5);

        let message = WebEventsMessage::sessions(vec![session], vec![reset, delta]);
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["type"], "sessions");
        assert!(value["sessions"].is_array());
        assert_eq!(value["sessions"][0]["client_lease_ms"], 120_000);
        assert_eq!(value["sessions"][0]["orphan_grace_ms"], 600_000);
        assert_eq!(value["terminals"][0]["reset"], true);
        assert_eq!(value["terminals"][0]["next_cursor"], 7);
        assert_eq!(value["terminals"][1]["reset"], false);
        assert_eq!(value["terminals"][1]["cursor"], 3);
        let round_trip: WebEventsMessage = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip.message_type, "sessions");
        assert_eq!(round_trip.terminals.len(), 2);
    }
}
