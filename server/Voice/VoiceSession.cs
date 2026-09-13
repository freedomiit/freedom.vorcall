using System.Buffers;
using System.Collections.Immutable;
using System.Net;
using System.Security.Cryptography;
using System.Threading.Channels;

namespace Vorcall.Server.Voice;

// One voice membership: the key handed to the client in VoiceReady, the cipher built from it,
// and the per-sender state the relay keeps. Replay, Bucket and ShareBucket belong to the receive
// loop and are touched from nowhere else; the address, the speaking flag, the share state and the
// moderation flags cross threads. The share state and the moderation flags are only ever written
// under the relay's gate.
public sealed class VoiceSession : IDisposable
{
    public const int KeyBytes = 32;

    // Roughly five times the 20 ms Opus cadence, so a client that bursts after a stall is not
    // punished while a flood still costs the relay a fixed amount of work.
    private const double PermitsPerSecond = 100;
    private const double BurstPermits = 200;

    // Half a second of the configured share rate, and never less than 1.5 MiB: a keyframe arrives
    // as one burst of fragments, and the budget exists to cap a runaway sender, not to shape it.
    private const double MinShareBurstBytes = 1.5 * 1024 * 1024;

    // Roughly a second of a 30 Mbit/s share at 1200 bytes a datagram. Past that the oldest frames
    // are worthless, so the queue drops them rather than the newest.
    private const int ShareQueueCapacity = 4096;

    private IPEndPoint? _address;
    private long _lastAudioTicks;
    private int _speaking;
    private bool _sharing;
    private bool _shareAudio;
    private bool _muted;
    private bool _deafened;
    private bool _priority;
    private ImmutableArray<VoiceSession> _watchers = [];
    private VoiceSession? _watching;
    private Channel<Outbound> _shareQueue;
    private long _queueDrops;

    internal VoiceSession(uint ssrc, long userId, long channelId, byte[] key, int shareMaxKbps)
    {
        Ssrc = ssrc;
        UserId = userId;
        ChannelId = channelId;
        Key = key;
        Cipher = new ChaCha20Poly1305(key);
        _shareQueue = CreateShareQueue();

        var bytesPerSecond = shareMaxKbps * 1000d / 8d;
        ShareBucket = new TokenBucket(bytesPerSecond, Math.Max(MinShareBurstBytes, bytesPerSecond / 2));
    }

    public uint Ssrc { get; }

    public long UserId { get; }

    public long ChannelId { get; }

    public byte[] Key { get; }

    // Null until a datagram sealed with this session's key arrives: the relay never sends to an
    // address it has not authenticated.
    public IPEndPoint? Address => Volatile.Read(ref _address);

    internal ReplayWindow Replay { get; } = new();

    internal TokenBucket Bucket { get; } = new(PermitsPerSecond, BurstPermits);

    // Bytes, not packets: share media is charged its datagram length and never touches Bucket.
    internal TokenBucket ShareBucket { get; }

    // Whether this session is screen sharing, and whether that share carries its own audio.
    public bool Sharing
    {
        get => Volatile.Read(ref _sharing);
        internal set => Volatile.Write(ref _sharing, value);
    }

    public bool ShareAudio
    {
        get => Volatile.Read(ref _shareAudio);
        internal set => Volatile.Write(ref _shareAudio, value);
    }

    // Server mute: the relay drops this session's audio before it is forwarded or counted as
    // speaking. Server deafen: the session is skipped as a recipient of everyone else's media.
    public bool Muted
    {
        get => Volatile.Read(ref _muted);
        internal set => Volatile.Write(ref _muted, value);
    }

    public bool Deafened
    {
        get => Volatile.Read(ref _deafened);
        internal set => Volatile.Write(ref _deafened, value);
    }

    // Carried, never acted on: the signalling side mirrors it into VoiceMember.priority instead of
    // keeping a second store of its own.
    public bool Priority
    {
        get => Volatile.Read(ref _priority);
        internal set => Volatile.Write(ref _priority, value);
    }

    // The sessions this share is forwarded to. Replaced under the relay's gate, never mutated,
    // and read as a snapshot by the sender worker: the interlocked write publishes the array, and
    // the worker reads the field again for every datagram it dequeues.
    internal ImmutableArray<VoiceSession> Watchers
    {
        get => _watchers;
        set => ImmutableInterlocked.InterlockedExchange(ref _watchers, value);
    }

    // The share this session is watching, if any: the only target a keyframe request may name.
    internal VoiceSession? Watching
    {
        get => Volatile.Read(ref _watching);
        set => Volatile.Write(ref _watching, value);
    }

    // Share media leaves the receive loop here and the session's own worker picks it up, so a
    // sharer's fan-out never delays anyone's audio.
    internal ChannelWriter<Outbound> ShareWriter => Volatile.Read(ref _shareQueue).Writer;

    // Frames this session's queue evicted because the worker could not keep up. Cumulative over
    // the session, across however many shares it ran, and reported with the channel's other drops.
    internal long QueueDrops => Interlocked.Read(ref _queueDrops);

    internal bool IsSpeaking => Volatile.Read(ref _speaking) != 0;

    internal long LastAudioTicks => Interlocked.Read(ref _lastAudioTicks);

    private ChaCha20Poly1305 Cipher { get; }

    // ChaCha20Poly1305 is one OpenSSL cipher context, and OpenSSL does not tolerate two threads
    // in one context: a watcher's cipher is driven by the receive loop (audio) and by a sharer's
    // worker (video) at once, and without this gate that shows up as a spurious tag failure at
    // best and a SIGSEGV that takes the whole server down at worst.
    private readonly Lock _cipherGate = new();

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
            lock (_cipherGate)
            {
                Cipher.Decrypt(nonce, ciphertext, tag, plaintext, associatedData);
            }

            return true;
        }
        catch (Exception ex) when (ex is CryptographicException or ObjectDisposedException)
        {
            return false;
        }
    }

    // The outbound half of the same pair, and it fails the same two ways: a disposed cipher means
    // the recipient was removed while this packet was in flight, and the provider itself can
    // refuse an operation. Both are one dropped copy of one datagram, counted as DropSealFailed,
    // never an error — this runs on the relay's receive loop, and a throw there faults
    // ExecuteAsync, which stops the host and every connected client with it.
    internal bool TrySeal(
        ReadOnlySpan<byte> nonce,
        ReadOnlySpan<byte> plaintext,
        Span<byte> ciphertext,
        Span<byte> tag,
        ReadOnlySpan<byte> associatedData)
    {
        try
        {
            lock (_cipherGate)
            {
                Cipher.Encrypt(nonce, plaintext, ciphertext, tag, associatedData);
            }

            return true;
        }
        catch (Exception ex) when (ex is CryptographicException or ObjectDisposedException)
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

    // A completed queue can never be written again, so a share that starts after one stopped gets
    // a fresh one. Called under the relay's gate, only when this session was not already sharing.
    internal ChannelReader<Outbound> RestartShareQueue()
    {
        var queue = CreateShareQueue();
        Volatile.Write(ref _shareQueue, queue);
        return queue.Reader;
    }

    // Harmless twice: the second call finds the queue already completed and says so.
    internal void CompleteShareQueue() => Volatile.Read(ref _shareQueue).Writer.TryComplete();

    public void Dispose()
    {
        lock (_cipherGate)
        {
            Cipher.Dispose();
        }
    }

    // An eviction is the one drop nobody else is in a position to see: it happens inside the
    // channel, on whichever thread was writing, so the rental goes back to the pool and the
    // session counts it here.
    private Channel<Outbound> CreateShareQueue()
        => Channel.CreateBounded<Outbound>(
            new BoundedChannelOptions(ShareQueueCapacity)
            {
                FullMode = BoundedChannelFullMode.DropOldest,
                SingleReader = true,
            },
            dropped =>
            {
                ArrayPool<byte>.Shared.Return(dropped.Buffer);
                Interlocked.Increment(ref _queueDrops);
            });

    // One queued datagram: the 19-byte header verbatim followed by its plaintext, in a buffer
    // rented from the shared pool that the sender worker returns once it has forwarded it.
    internal readonly record struct Outbound(byte[] Buffer, int PlaintextLength);
}
