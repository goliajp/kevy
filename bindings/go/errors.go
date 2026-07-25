package kevy

import (
	"errors"
	"fmt"
	"strings"
)

// ErrorKind is the canonical error taxonomy (contract §2.2). The variant
// identity survives so callers can branch on it via errors.As(&KevyError).
type ErrorKind int

const (
	// KindStore is a structured store-semantic error; see StoreErr.
	KindStore ErrorKind = iota
	// KindIo is an OS/transport failure (file, socket, AOF).
	KindIo
	// KindProtocol is a RESP-level error: a server -ERR reply (text
	// preserved verbatim) or a malformed / unexpected reply shape.
	KindProtocol
	// KindReadOnly means the write was rejected by a read-only replica.
	KindReadOnly
	// KindInvalidInput is a bad argument rejected before touching state.
	KindInvalidInput
	// KindNotFound means a named object (index, view, key) does not exist.
	KindNotFound
	// KindUnsupported means the op is unavailable on this backend/build.
	KindUnsupported
	// KindTimedOut means a bounded blocking call ran out its timeout.
	KindTimedOut
	// KindClosed means the connection / in-process bus is gone (EOF).
	KindClosed
)

// StoreErrorKind is the structured store-semantic error carried inside a
// KevyError with Kind == KindStore (contract §2.3).
type StoreErrorKind int

const (
	// StoreOK is the zero value (no store error).
	StoreOK StoreErrorKind = iota
	// StoreWrongType — key holds a different type than the command expects.
	StoreWrongType
	// StoreNotInteger — value is not a base-10 integer (INCR family).
	StoreNotInteger
	// StoreOverflow — the result would overflow i64.
	StoreOverflow
	// StoreOutOfRange — index outside the collection (LSET).
	StoreOutOfRange
	// StoreNoSuchKey — key does not exist where the command requires one.
	StoreNoSuchKey
	// StoreNotFloat — value is not a valid float (INCRBYFLOAT).
	StoreNotFloat
	// StoreOutOfMemory — maxmemory exceeded under NoEviction.
	StoreOutOfMemory
)

func (s StoreErrorKind) String() string {
	switch s {
	case StoreWrongType:
		return "WrongType"
	case StoreNotInteger:
		return "NotInteger"
	case StoreOverflow:
		return "Overflow"
	case StoreOutOfRange:
		return "OutOfRange"
	case StoreNoSuchKey:
		return "NoSuchKey"
	case StoreNotFloat:
		return "NotFloat"
	case StoreOutOfMemory:
		return "OutOfMemory"
	default:
		return "OK"
	}
}

// KevyError is the single error type of the Go client. It implements
// error and is inspectable via errors.As (contract §2.1, Go row).
type KevyError struct {
	// Kind is the error taxonomy variant.
	Kind ErrorKind
	// Msg carries the wire/description text for Protocol / InvalidInput /
	// NotFound / Unsupported.
	Msg string
	// StoreErr is set (non-OK) when Kind == KindStore.
	StoreErr StoreErrorKind
	// Cause wraps an underlying error for KindIo (returned by Unwrap).
	Cause error
}

func (e *KevyError) Error() string {
	switch e.Kind {
	case KindStore:
		return "store error: " + e.StoreErr.String()
	case KindIo:
		if e.Cause != nil {
			return "io error: " + e.Cause.Error()
		}
		return "io error: " + e.Msg
	case KindProtocol:
		return "protocol error: " + e.Msg
	case KindReadOnly:
		return "READONLY You can't write against a read only replica"
	case KindInvalidInput:
		return "invalid input: " + e.Msg
	case KindNotFound:
		return "not found: " + e.Msg
	case KindUnsupported:
		return "unsupported: " + e.Msg
	case KindTimedOut:
		return "timed out"
	case KindClosed:
		return "connection closed"
	default:
		return "kevy error"
	}
}

// Unwrap exposes the underlying I/O error for errors.Is / errors.As.
func (e *KevyError) Unwrap() error { return e.Cause }

// IsKind reports whether err is a *KevyError of the given kind.
func IsKind(err error, kind ErrorKind) bool {
	var ke *KevyError
	return errors.As(err, &ke) && ke.Kind == kind
}

// StoreErrorOf returns (kind, true) when err is a KindStore *KevyError.
func StoreErrorOf(err error) (StoreErrorKind, bool) {
	var ke *KevyError
	if errors.As(err, &ke) && ke.Kind == KindStore {
		return ke.StoreErr, true
	}
	return StoreOK, false
}

// --- constructors -------------------------------------------------------

func errProtocol(format string, a ...any) *KevyError {
	return &KevyError{Kind: KindProtocol, Msg: fmt.Sprintf(format, a...)}
}

func errInvalidInput(msg string) *KevyError {
	return &KevyError{Kind: KindInvalidInput, Msg: msg}
}

func errUnsupported(msg string) *KevyError {
	return &KevyError{Kind: KindUnsupported, Msg: msg}
}

func errIo(cause error) *KevyError {
	return &KevyError{Kind: KindIo, Cause: cause, Msg: cause.Error()}
}

func errStore(s StoreErrorKind) *KevyError {
	return &KevyError{Kind: KindStore, StoreErr: s}
}

func errClosed() *KevyError { return &KevyError{Kind: KindClosed} }

// unexpectedReply builds a Protocol error naming the actual variant, so a
// shape mismatch is diagnosable without wire logging (mirrors the Rust
// reference's `unexpected`).
func unexpectedReply(r Reply) *KevyError {
	return errProtocol("unexpected RESP reply variant: %s", r.shape())
}

// classifyStoreError maps a server error frame's text onto the structured
// store-error taxonomy. It returns (kind, true) for a recognised store
// error — the caller (replyErr) then wraps it as KindStore (contract §6:
// "wrong-type op surfaces Store(WrongType)") — and (_, false) for anything
// else, which replyErr keeps verbatim as a Protocol error (contract §2.2:
// remote -ERR → Protocol).
func classifyStoreError(text []byte) (StoreErrorKind, bool) {
	s := string(text)
	switch {
	case strings.HasPrefix(s, "WRONGTYPE"):
		return StoreWrongType, true
	case strings.HasPrefix(s, "OOM"):
		return StoreOutOfMemory, true
	case strings.Contains(s, "is not an integer"):
		return StoreNotInteger, true
	case strings.Contains(s, "is not a valid float"):
		return StoreNotFloat, true
	case strings.Contains(s, "would overflow"):
		return StoreOverflow, true
	case strings.Contains(s, "no such key"):
		return StoreNoSuchKey, true
	case strings.Contains(s, "index out of range"):
		return StoreOutOfRange, true
	default:
		return StoreOK, false
	}
}
