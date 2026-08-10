/// Health check response
export interface HealthResponse {
  status: string;
  version: string;
}

/// Readiness check response
export interface ReadinessResponse {
  ready: boolean;
  reason?: string;
}

/// Model information
export interface ModelInfo {
  id: string;
  name: string;
  language?: string;
  size_bytes?: number;
  loaded: boolean;
  model_id?: string;
}

/// Batch transcription request options
export interface TranscriptionRequest {
  model?: string;
  language?: string;
  prompt?: string;
  temperature?: number;
}

/// A segment of transcribed text
export interface TranscriptionSegment {
  text: string;
  start_ms: number;
  end_ms: number;
  probability: number;
}

/// Transcription result
export interface TranscriptionResult {
  id: string;
  text: string;
  language: string;
  duration_secs: number;
  segments: TranscriptionSegment[];
}

/// Realtime session configuration
export interface RealtimeConfig {
  model?: string;
  language?: string;
  sample_rate: number;
  channels: number;
  sample_format: 'signed_16bit_le';
}

/// Messages from server over WebSocket
export type RealtimeMessage =
  | { type: 'started'; session_id: string }
  | { type: 'partial'; text: string }
  | { type: 'final'; text: string; segments: TranscriptionSegment[] }
  | { type: 'completed'; session_id: string }
  | { type: 'error'; code: string; message: string };

/// Client options
export interface SttClientOptions {
  baseUrl: string;
  timeout?: number;
}
