import type {
  HealthResponse,
  ReadinessResponse,
  ModelInfo,
  TranscriptionRequest,
  TranscriptionResult,
  RealtimeConfig,
  RealtimeMessage,
  SttClientOptions,
} from './types';

/**
 * stt-server SDK client.
 *
 * Provides typed access to batch and realtime transcription endpoints.
 */
export class SttClient {
  private baseUrl: string;
  private timeout: number;

  constructor(options: SttClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, '');
    this.timeout = options.timeout ?? 30_000;
  }

  // ── Health ────────────────────────────────────────────────

  /** Check server health. */
  async health(): Promise<HealthResponse> {
    const res = await fetch(`${this.baseUrl}/v1/health`);
    if (!res.ok) throw new Error(`Health check failed: ${res.status}`);
    return res.json();
  }

  /** Check server readiness. */
  async readiness(): Promise<ReadinessResponse> {
    const res = await fetch(`${this.baseUrl}/v1/readiness`);
    if (!res.ok) throw new Error(`Readiness check failed: ${res.status}`);
    return res.json();
  }

  // ── Models ────────────────────────────────────────────────

  /** List all registered models. */
  async listModels(): Promise<ModelInfo[]> {
    const res = await fetch(`${this.baseUrl}/v1/models`);
    if (!res.ok) throw new Error(`List models failed: ${res.status}`);
    return res.json();
  }

  /** Get the currently selected default model. */
  async getSelectedModel(): Promise<{ selected_model_id: string | null }> {
    const res = await fetch(`${this.baseUrl}/v1/models/selected`);
    if (!res.ok) throw new Error(`Get selected model failed: ${res.status}`);
    return res.json();
  }

  /** Select a loaded model as the default. */
  async selectModel(modelId: string): Promise<{ status: string; model_id: string }> {
    const res = await fetch(`${this.baseUrl}/v1/models/select`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model_id: modelId }),
    });
    if (!res.ok) throw new Error(`Select model failed: ${res.status}`);
    return res.json();
  }

  // ── Batch Transcription ───────────────────────────────────

  /** Transcribe a WAV audio buffer. */
  async transcribe(
    audioBuffer: ArrayBuffer,
    options?: TranscriptionRequest,
  ): Promise<TranscriptionResult> {
    const params = new URLSearchParams();
    if (options?.model) params.set('model', options.model);
    if (options?.language) params.set('language', options.language);
    if (options?.prompt) params.set('prompt', options.prompt);
    if (options?.temperature !== undefined) params.set('temperature', String(options.temperature));

    const url = `${this.baseUrl}/v1/transcriptions${params.toString() ? '?' + params.toString() : ''}`;

    const res = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: audioBuffer,
      signal: AbortSignal.timeout(this.timeout),
    });

    if (!res.ok) {
      const err = await res.json().catch(() => ({ code: 'UNKNOWN', message: res.statusText }));
      throw new Error(`Transcription failed: [${err.code}] ${err.message}`);
    }

    return res.json();
  }

  // ── Realtime Transcription ────────────────────────────────

  /** Open a realtime transcription session. Returns a handler for streaming. */
  realtime(config: RealtimeConfig): RealtimeSession {
    const wsUrl = this.baseUrl.replace(/^http/, 'ws') + '/v1/realtime/transcriptions';
    return new RealtimeSession(wsUrl, config);
  }
}

/**
 * Manages a WebSocket-based realtime transcription session.
 */
export class RealtimeSession {
  private ws: WebSocket | null = null;
  private url: string;
  private config: RealtimeConfig;
  private handlers: Map<string, ((msg: RealtimeMessage) => void)[]> = new Map();
  private sessionId: string | null = null;

  constructor(url: string, config: RealtimeConfig) {
    this.url = url;
    this.config = config;
  }

  /** Connect and start the session. */
  connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.ws = new WebSocket(this.url);

      this.ws.onopen = () => {
        // Send start message
        this.ws!.send(
          JSON.stringify({
            type: 'start',
            config: this.config,
          }),
        );
        resolve();
      };

      this.ws.onmessage = (event) => {
        try {
          const msg: RealtimeMessage = JSON.parse(event.data as string);

          if (msg.type === 'started') {
            this.sessionId = msg.session_id;
          }

          this.emit(msg.type, msg);
        } catch {
          // Ignore parse errors
        }
      };

      this.ws.onerror = (event) => {
        reject(new Error(`WebSocket error: ${event}`));
      };

      this.ws.onclose = () => {
        this.emit('closed', { type: 'closed' } as any);
      };
    });
  }

  /** Send audio samples (f32 PCM). */
  sendAudio(samples: Float32Array): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error('Not connected');
    }

    // Convert f32 to s16le bytes
    const bytes = new ArrayBuffer(samples.length * 2);
    const view = new DataView(bytes);
    for (let i = 0; i < samples.length; i++) {
      const s16 = Math.max(-32768, Math.min(32767, Math.round(samples[i] * 32768)));
      view.setInt16(i * 2, s16, true);
    }

    this.ws.send(new Uint8Array(bytes));
  }

  /** Complete the session and get final result. */
  complete(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error('Not connected');
    }
    this.ws.send(JSON.stringify({ type: 'complete' }));
  }

  /** Cancel the session. */
  cancel(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }
    this.ws.send(JSON.stringify({ type: 'cancel' }));
  }

  /** Register an event handler. */
  on(event: string, handler: (msg: RealtimeMessage) => void): void {
    const list = this.handlers.get(event) ?? [];
    list.push(handler);
    this.handlers.set(event, list);
  }

  /** Close the session. */
  close(): void {
    this.cancel();
    this.ws?.close();
    this.ws = null;
  }

  /** Get the session ID (available after connect). */
  getSessionId(): string | null {
    return this.sessionId;
  }

  private emit(event: string, msg: RealtimeMessage): void {
    const list = this.handlers.get(event) ?? [];
    for (const handler of list) {
      handler(msg);
    }
  }
}
