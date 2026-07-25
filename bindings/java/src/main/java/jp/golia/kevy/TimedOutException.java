// TimedOutException — a bounded blocking call ran out its timeout
// (client-contract §2.2 TIMED_OUT).
package jp.golia.kevy;

public final class TimedOutException extends KevyException {
    public TimedOutException(String message) {
        super(ErrorKind.TIMED_OUT, message);
    }
}
