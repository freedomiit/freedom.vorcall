using System.Net;
using System.Security.Cryptography;

namespace Vorcall.Server.Voice;

// One voice membership: the key handed to the client in VoiceReady, the cipher built from it,
// and the per-sender state the relay keeps. Replay and Bucket belong to the receive loop and
// are touched from nowhere else; the address and the speaking flag cross threads.
public sealed class VoiceSession : IDisposable
{
    public const int KeyBytes = 32;

    // Roughly five times the 20 ms Opus cadence, so a client that bursts after a stall is not
    // punished while a flood still costs the relay a fixed amount of work.
    private const double PermitsPerSecond = 100;
    private const double BurstPermits = 200;

    private IPEndPoint? _address;
    private long _lastAudioTicks;
    private int _speaking;

    internal VoiceSession(uint ssrc, long userId, string roomId, byte[] key)
    {
        Ssrc = ssrc;
        UserId = userId;
        RoomId = roomId;
        Key = key;
        Cipher = new ChaCha20Poly1305(key);
    }

    public uint Ssrc { get; }

    public long UserId { get; }

    public string RoomId { get; }

    public byte[] Key { get; }

    // Null until a datagram sealed with this session's key arrives: the relay never sends to an
    // address it has not authenticated.
    public IPEndPoint? Address => Volatile.Read(ref _address);

    internal ReplayWindow Replay { get; } = new();

    internal TokenBucket Bucket { get; } = new(PermitsPerSecond, BurstPermits);

    internal bool IsSpeaking => Volatile.Read(ref _speaking) != 0;

    internal long LastAudioTicks => Interlocked.Read(ref _lastAudioTicks);

    private ChaCha20Poly1305 Cipher { get; }

    // A tag mismatch is the ordinary fate of a forged or corrupted datagram, and a disposed
    // cipher means the session was removed while this packet was in flight. Both are drops, and
    // neither may take the receive loop down.
    internal bool TryOpen(
        ReadOnlySpan<byte> nonce,
        ReadOnlySpan<byte> ciphertext,
        ReadOnlySpan<byte> tag,
        Span<byte> plaintext,
        ReadOnlySpan<byte> associatedData)
    {
        try
        {
            Cipher.Decrypt(nonce, ciphertext, tag, plaintext, associatedData);
            return true;
        }
        catch (Exception ex) when (ex is CryptographicException or ObjectDisposedException)
        {
            return false;
        }
    }

    internal bool TrySeal(
        ReadOnlySpan<byte> nonce,
        ReadOnlySpan<byte> plaintext,
        Span<byte> ciphertext,
        Span<byte> tag,
        ReadOnlySpan<byte> associatedData)
    {
        try
        {
            Cipher.Encrypt(nonce, plaintext, ciphertext, tag, associatedData);
            return true;
        }
        catch (ObjectDisposedException)
        {
            return false;
        }
    }

    // Any change is accepted: a NAT rebinding or a roaming client keeps talking. True when the
    // session had already been reached at a different address.
    internal bool LearnAddress(IPEndPoint address)
    {
        var current = Volatile.Read(ref _address);
        if (current is not null && current.Equals(address))
        {
            return false;
        }

        Volatile.Write(ref _address, address);
        return current is not null;
    }

    // True when this packet opened a talk spurt. The timestamp is published before the flag, so
    // housekeeping can never see a speaking session with a stale silence deadline.
    internal bool MarkAudio(long nowTicks)
    {
        Interlocked.Exchange(ref _lastAudioTicks, nowTicks);
        return Interlocked.Exchange(ref _speaking, 1) == 0;
    }

    internal bool StopSpeaking() => Interlocked.Exchange(ref _speaking, 0) == 1;

    public void Dispose() => Cipher.Dispose();
}
