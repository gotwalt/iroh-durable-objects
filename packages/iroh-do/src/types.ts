// The public types of the connection and stream API. They follow `@number0/iroh` 1.1.0
// (README.md, "Differences from @number0/iroh"), and are written here rather than taken from
// the wasm-bindgen output: those classes also carry `free()` and `[Symbol.dispose]`, which
// would release objects the node still uses, and Rust parameter names.

/** An endpoint's identifier, a 32-byte ed25519 public key. */
export interface EndpointId {
  /** Whether both ids are equal. */
  equals(other: EndpointId): boolean;
  /** The 32 bytes. */
  toBytes(): Uint8Array;
  /** Short form for logs. */
  fmtShort(): string;
  /** The default text form (`fromString` parses it). Never for URLs or DNS labels. */
  toString(): string;
  /** z-base-32, the form in relay URLs and DNS labels (design note 004). */
  toZ32(): string;
}

/** The constructors of `EndpointId`. */
export interface EndpointIdStatic {
  /** Parses the default text form, as iroh's `FromStr` does. Not for URLs or DNS labels. */
  fromString(s: string): EndpointId;
  /** Parses z-base-32, 52 characters. */
  fromZ32(s: string): EndpointId;
  /** From the 32 bytes; fails if they are not a valid key. */
  fromBytes(bytes: Uint8Array): EndpointId;
}

/** An endpoint's id and the relay URL it can be reached at. */
export interface EndpointAddr {
  id(): EndpointId;
  /** Always empty: an object has no IP transport. */
  directAddresses(): string[];
  relayUrl(): string | null;
  equal(other: EndpointAddr): boolean;
  /** `<id> relay=<url>`, the id as `EndpointId.toString` writes it. */
  toString(): string;
}

/** The constructor of `EndpointAddr`. `addresses` is accepted and ignored. */
export type EndpointAddrConstructor = new (
  id: EndpointId,
  relayUrl?: string | null,
  addresses?: string[] | null,
) => EndpointAddr;

/** The sending half of a stream. Errors are `Error`s named after the Rust error type. */
export interface SendStream {
  /** Writes some of `buf`; resolves with the number of bytes written. */
  write(buf: Uint8Array): Promise<number>;
  /** Writes all of `buf`. */
  writeAll(buf: Uint8Array): Promise<void>;
  /** Finishes the stream after the writes already called: the peer reads to its end. */
  finish(): Promise<void>;
  /** Abandons the stream with an application error code; a pending write fails at once. */
  reset(errorCode: bigint): Promise<void>;
  /** Resolves with null once the peer has received everything, or with its stop code. */
  stopped(): Promise<number | null>;
  setPriority(priority: number): Promise<void>;
  priority(): Promise<number>;
  /** The stream's id, as text. */
  id(): Promise<string>;
}

/** The receiving half of a stream. */
export interface RecvStream {
  /** Reads up to `sizeLimit` bytes; an empty array at the end of the stream. */
  read(sizeLimit: number): Promise<Uint8Array>;
  /** Reads exactly `size` bytes; fails if the stream ends first. */
  readExact(size: number): Promise<Uint8Array>;
  /** Reads to the end of the stream; fails beyond `sizeLimit` bytes. */
  readToEnd(sizeLimit: number): Promise<Uint8Array>;
  /** Stops the stream: the peer's writes fail with `errorCode`; a pending read fails at once. */
  stop(errorCode: bigint): Promise<void>;
  /** Resolves with the peer's reset code, or null if the stream ended otherwise. */
  receivedReset(): Promise<number | null>;
  /** Bytes read so far. */
  bytesRead(): Promise<number>;
  /** The stream's id, as text. */
  id(): Promise<string>;
}

/** A bidirectional stream. */
export interface BiStream {
  readonly send: SendStream;
  readonly recv: RecvStream;
}

/** A QUIC connection. */
export interface Connection {
  /** The negotiated ALPN. */
  alpn(): Uint8Array;
  /** The peer's id. */
  remoteId(): EndpointId;
  /** `Client` for a connection this endpoint dialed, `Server` for one it accepted. */
  side(): "Client" | "Server";
  /** Unique among this endpoint's live connections. */
  stableId(): number;
  acceptBi(): Promise<BiStream>;
  openBi(): Promise<BiStream>;
  acceptUni(): Promise<RecvStream>;
  openUni(): Promise<SendStream>;
  /** Closes the connection with an application error code and reason. */
  close(errorCode: bigint, reason: Uint8Array): void;
  /** Resolves with the reason once the connection is closed, by either side. */
  closed(): Promise<string>;
  /** The close reason, or null while open. */
  closeReason(): string | null;
}

/** This object's endpoint, as the app sees it (`IrohDurableObject.endpoint`). */
export interface Endpoint {
  id(): EndpointId;
  /** The id and the relay URL, which the object serves itself. */
  addr(): EndpointAddr;
  /** The peers holding a relay socket to this object: the only peers `connect` can reach. */
  peers(): EndpointId[];
  /**
   * Dials `peer` over its relay socket to this object (dial-back), binding the endpoint if
   * needed. Fails at once with a `ConnectError` if the peer holds no socket: the object
   * never dials out.
   */
  connect(peer: EndpointId | EndpointAddr, alpn: string | Uint8Array): Promise<Connection>;
}
