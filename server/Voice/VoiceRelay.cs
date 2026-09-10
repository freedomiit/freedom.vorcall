using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Collections.Immutable;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;

namespace Vorcall.Server.Voice;

// The media path: one UDP socket, one receive loop, one key per voice session. Every datagram
// is authenticated before anything is forwarded, and the relay only ever sends to an address
// that a session's own key has already been proved from, so the socket cannot be used to
// bounce traffic at a third party.
public sealed class VoiceRelay(VoiceOptions options, ILogger<VoiceRelay> logger) : BackgroundService
{
    private const int ReceiveBufferBytes = 1024 * 1024;
    private const int DatagramBufferBytes = 2048;
    private const int MaxPlaintextBytes = MediaHeader.MaxDatagram - MediaHeader.MinDatagram;

    private static readonly TimeSpan SpeakingTimeout = TimeSpan.FromMilliseconds(250);
    private static readonly TimeSpan HousekeepingInterval = TimeSpan.FromMilliseconds(100);
    private static readonly TimeSpan MetricsInterval = TimeSpan.FromSeconds(30);

    private readonly ConcurrentDictionary<uint, VoiceSession> _sessions = new();

    // Copy-on-write under _gate so the receive loop forwards over a snapshot without locking.
    private readonly ConcurrentDictionary<string, ImmutableArray<VoiceSession>> _rooms = new();
    private readonly ConcurrentDictionary<string, RoomCounters> _counters = new();
    private readonly Lock _gate = new();

    // These three drops happen before a datagram is attached to any session, so there is no
    // room to charge them to: they are relay-wide and repeat in every room's line.
    private long _dropSize;
    private long _dropHeader;
    private long _dropUnknownSsrc;

    private volatile Socket? _socket;

    // Raised from the receive loop and the housekeeping tick: handlers must be fast and must
    // not throw.
    public event Action<string, long, bool>? SpeakingChanged;

    public bool IsAvailable => _socket is not null;

    public string AdvertisedHost => options.Host;

    public int Port => options.Port;

    public VoiceSession CreateSession(string roomId, long userId)
    {
        var key = RandomNumberGenerator.GetBytes(VoiceSession.KeyBytes);
        VoiceSession session;
        lock (_gate)
        {
            uint ssrc;
            do
            {
                ssrc = NextSsrc();
            }
            while (ssrc == 0 || _sessions.ContainsKey(ssrc));

            session = new VoiceSession(ssrc, userId, roomId, key);
            _sessions[ssrc] = session;
            _rooms[roomId] = _rooms.TryGetValue(roomId, out var members) ? members.Add(session) : [session];
        }

        logger.LogDebug("Voice session {Ssrc} opened for user {UserId} in room {RoomId}", session.Ssrc, userId, roomId);
        return session;
    }

    public void RemoveSession(uint ssrc)
    {
        VoiceSession? session;
        lock (_gate)
        {
            if (!_sessions.TryRemove(ssrc, out session))
            {
                return;
            }

            if (_rooms.TryGetValue(session.RoomId, out var members))
            {
                var remaining = members.Remove(session);
                if (remaining.IsEmpty)
                {
                    _rooms.TryRemove(session.RoomId, out _);
                }
                else
                {
                    _rooms[session.RoomId] = remaining;
                }
            }
        }

        if (session.StopSpeaking())
        {
            RaiseSpeakingChanged(session, false);
        }

        session.Dispose();
        logger.LogDebug("Voice session {Ssrc} closed for user {UserId} in room {RoomId}", ssrc, session.UserId, session.RoomId);
    }

    public override Task StartAsync(CancellationToken cancellationToken)
    {
        if (!options.Enabled)
        {
            logger.LogInformation("Voice relay disabled by configuration");
            return base.StartAsync(cancellationToken);
        }

        // Both failures are configuration or platform problems, not runtime conditions: the
        // host must not come up half working, exactly like a missing signing key.
        if (!ChaCha20Poly1305.IsSupported)
        {
            throw new InvalidOperationException(
                "Voice requires ChaCha20-Poly1305, which this platform does not provide. Set 'Vorcall:VoiceEnabled' to false to run without voice.");
        }

        var socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        try
        {
            socket.ReceiveBufferSize = ReceiveBufferBytes;
            socket.Bind(new IPEndPoint(IPAddress.Any, options.Port));
        }
        catch (Exception ex) when (ex is SocketException or ArgumentException)
        {
            socket.Dispose();
            throw new InvalidOperationException(
                $"Failed to bind the voice relay to 0.0.0.0:{options.Port} (configuration 'Vorcall:VoicePort').",
                ex);
        }

        _socket = socket;
        logger.LogInformation("Voice relay listening on 0.0.0.0:{Port}", options.Port);
        return base.StartAsync(cancellationToken);
    }

    public override async Task StopAsync(CancellationToken cancellationToken)
    {
        // Closing the socket is what releases the pending receive, whatever the token does.
        var socket = _socket;
        _socket = null;
        socket?.Dispose();
        await base.StopAsync(cancellationToken);
    }

    protected override Task ExecuteAsync(CancellationToken stoppingToken)
    {
        var socket = _socket;
        return socket is null
            ? Task.CompletedTask
            : Task.WhenAll(ReceiveLoopAsync(socket, stoppingToken), HousekeepingAsync(stoppingToken));
    }

    private static uint NextSsrc()
    {
        Span<byte> bytes = stackalloc byte[sizeof(uint)];
        RandomNumberGenerator.Fill(bytes);
        return BinaryPrimitives.ReadUInt32BigEndian(bytes);
    }

    private static bool TrySeal(VoiceSession recipient, byte[] datagram, byte[] plaintext, int plaintextLength)
    {
        var header = datagram.AsSpan(0, MediaHeader.Length);
        return recipient.TrySeal(
            MediaHeader.NonceOf(header),
            plaintext.AsSpan(0, plaintextLength),
            datagram.AsSpan(MediaHeader.Length, plaintextLength),
            datagram.AsSpan(MediaHeader.Length + plaintextLength, MediaHeader.TagLength),
            header);
    }

    private async Task ReceiveLoopAsync(Socket socket, CancellationToken stoppingToken)
    {
        var buffer = new byte[DatagramBufferBytes];
        var scratch = new byte[MediaHeader.MaxDatagram];
        var plaintext = new byte[MaxPlaintextBytes];
        EndPoint from = new IPEndPoint(IPAddress.Any, 0);

        while (!stoppingToken.IsCancellationRequested)
        {
            SocketReceiveFromResult received;
            try
            {
                received = await socket.ReceiveFromAsync(buffer, SocketFlags.None, from, stoppingToken);
            }
            catch (SocketException ex)
            {
                // A client that went away answers with ICMP port unreachable, which surfaces
                // here as ConnectionReset on a connectionless socket that stays perfectly
                // usable. Never a warning: any peer can provoke it.
                logger.LogDebug(ex, "Voice receive failed with {SocketError}", ex.SocketErrorCode);
                continue;
            }
            catch (Exception ex) when (ex is ObjectDisposedException or OperationCanceledException)
            {
                return;
            }

            if (!TryAuthenticate(buffer, received.ReceivedBytes, received.RemoteEndPoint, plaintext, out var inbound))
            {
                continue;
            }

            if (inbound.Header.Type == MediaHeader.TypeAudio)
            {
                if (inbound.Session.MarkAudio(Stopwatch.GetTimestamp()))
                {
                    // The session was looked up before RemoveSession could have taken it out, so
                    // a blind raise here can land after the room already saw VoiceMemberLeft and
                    // leave a talker nobody ever silences. A removed session simply goes quiet.
                    if (_sessions.ContainsKey(inbound.Session.Ssrc))
                    {
                        RaiseSpeakingChanged(inbound.Session, true);
                    }
                    else
                    {
                        inbound.Session.StopSpeaking();
                    }
                }

                await ForwardAsync(socket, inbound, buffer, scratch, plaintext, stoppingToken);
            }
            else
            {
                await PongAsync(socket, inbound, scratch, plaintext, stoppingToken);
            }
        }
    }

    // The whole inbound pipeline up to the point where the datagram has earned the right to be
    // acted on. Synchronous on purpose: nothing may be forwarded before it authenticated.
    private bool TryAuthenticate(byte[] buffer, int length, EndPoint from, byte[] plaintext, out Inbound inbound)
    {
        inbound = default;
        if (length is < MediaHeader.MinDatagram or > MediaHeader.MaxDatagram)
        {
            Interlocked.Increment(ref _dropSize);
            return false;
        }

        var datagram = buffer.AsSpan(0, length);
        if (!MediaHeader.TryParse(datagram, out var header))
        {
            Interlocked.Increment(ref _dropHeader);
            return false;
        }

        if (!_sessions.TryGetValue(header.Ssrc, out var session))
        {
            Interlocked.Increment(ref _dropUnknownSsrc);
            return false;
        }

        // Unreachable on an IPv4 datagram socket, but the address is what the relay learns from
        // and it is never assumed.
        if (from is not IPEndPoint address)
        {
            Interlocked.Increment(ref _dropHeader);
            return false;
        }

        var counters = _counters.GetOrAdd(session.RoomId, static _ => new RoomCounters());
        if (!session.Bucket.TryTake(Stopwatch.GetTimestamp()))
        {
            Interlocked.Increment(ref counters.DropRate);
            return false;
        }

        var body = datagram[MediaHeader.Length..];
        var plaintextLength = body.Length - MediaHeader.TagLength;
        if (!session.TryOpen(
            MediaHeader.NonceOf(datagram),
            body[..plaintextLength],
            body[plaintextLength..],
            plaintext.AsSpan(0, plaintextLength),
            datagram[..MediaHeader.Length]))
        {
            Interlocked.Increment(ref counters.DropBadTag);
            return false;
        }

        // Only an authenticated sequence number may move the window: otherwise anyone could
        // push it forward with a forged datagram and lock the real sender out.
        if (!session.Replay.Accept(header.Seq))
        {
            Interlocked.Increment(ref counters.DropReplay);
            return false;
        }

        Interlocked.Increment(ref counters.PacketsIn);
        Interlocked.Add(ref counters.BytesIn, length);

        if (session.LearnAddress(address))
        {
            logger.LogDebug("Voice session of user {UserId} in room {RoomId} moved to another address", session.UserId, session.RoomId);
        }

        inbound = new Inbound(session, counters, header, plaintextLength);
        return true;
    }

    // Selective forwarding: the header travels verbatim, the payload is re-sealed with each
    // recipient's own key. The sender never gets its own audio back.
    private async Task ForwardAsync(Socket socket, Inbound inbound, byte[] buffer, byte[] scratch, byte[] plaintext, CancellationToken stoppingToken)
    {
        if (!_rooms.TryGetValue(inbound.Session.RoomId, out var members))
        {
            return;
        }

        buffer.AsSpan(0, MediaHeader.Length).CopyTo(scratch);
        var length = MediaHeader.Length + inbound.PlaintextLength + MediaHeader.TagLength;
        var counters = inbound.Counters;

        foreach (var member in members)
        {
            if (member.Ssrc == inbound.Session.Ssrc)
            {
                continue;
            }

            if (member.Address is not { } destination)
            {
                Interlocked.Increment(ref counters.DropNoAddress);
                continue;
            }

            // A false seal means the member was removed while this packet was in flight.
            if (TrySeal(member, scratch, plaintext, inbound.PlaintextLength))
            {
                await SendAsync(socket, scratch, length, destination, counters, stoppingToken);
            }
        }
    }

    private async Task PongAsync(Socket socket, Inbound inbound, byte[] scratch, byte[] plaintext, CancellationToken stoppingToken)
    {
        var counters = inbound.Counters;
        if (inbound.Session.Address is not { } destination)
        {
            Interlocked.Increment(ref counters.DropNoAddress);
            return;
        }

        // Same key as the ping, so the high seq bit is what keeps the two nonces apart.
        var header = inbound.Header with
        {
            Type = MediaHeader.TypePong,
            Seq = inbound.Header.Seq | MediaHeader.PongSeqBit,
        };
        header.Write(scratch);

        if (TrySeal(inbound.Session, scratch, plaintext, inbound.PlaintextLength))
        {
            var length = MediaHeader.Length + inbound.PlaintextLength + MediaHeader.TagLength;
            await SendAsync(socket, scratch, length, destination, counters, stoppingToken);
        }
    }

    private async Task SendAsync(Socket socket, byte[] datagram, int length, IPEndPoint destination, RoomCounters counters, CancellationToken stoppingToken)
    {
        try
        {
            var sent = await socket.SendToAsync(datagram.AsMemory(0, length), SocketFlags.None, destination, stoppingToken);
            Interlocked.Increment(ref counters.PacketsOut);
            Interlocked.Add(ref counters.BytesOut, sent);
        }
        catch (SocketException ex)
        {
            logger.LogDebug(ex, "Voice send failed with {SocketError}", ex.SocketErrorCode);
        }
        catch (Exception ex) when (ex is ObjectDisposedException or OperationCanceledException)
        {
            // Shutting down; the receive loop notices the same thing and leaves.
        }
    }

    private async Task HousekeepingAsync(CancellationToken stoppingToken)
    {
        using var timer = new PeriodicTimer(HousekeepingInterval);
        var lastMetrics = Stopwatch.GetTimestamp();

        try
        {
            while (await timer.WaitForNextTickAsync(stoppingToken))
            {
                var now = Stopwatch.GetTimestamp();
                ExpireSpeaking(now);

                if (Stopwatch.GetElapsedTime(lastMetrics, now) >= MetricsInterval)
                {
                    lastMetrics = now;
                    LogMetrics();
                }
            }
        }
        catch (OperationCanceledException)
        {
        }
    }

    private void ExpireSpeaking(long now)
    {
        foreach (var (_, session) in _sessions)
        {
            if (session.IsSpeaking
                && Stopwatch.GetElapsedTime(session.LastAudioTicks, now) > SpeakingTimeout
                && session.StopSpeaking())
            {
                RaiseSpeakingChanged(session, false);
            }
        }
    }

    private void LogMetrics()
    {
        var dropSize = Interlocked.Read(ref _dropSize);
        var dropHeader = Interlocked.Read(ref _dropHeader);
        var dropUnknownSsrc = Interlocked.Read(ref _dropUnknownSsrc);

        foreach (var (roomId, members) in _rooms)
        {
            if (members.IsEmpty || !_counters.TryGetValue(roomId, out var counters))
            {
                continue;
            }

            logger.LogInformation(
                "Voice room {RoomId}: {Sessions} sessions, {PacketsIn} packets in, {PacketsOut} out, {BytesIn} bytes in, {BytesOut} out; drops: {DropSize} size, {DropHeader} header, {DropUnknownSsrc} unknown ssrc, {DropRate} rate, {DropBadTag} bad tag, {DropReplay} replay, {DropNoAddress} no address",
                roomId,
                members.Length,
                Interlocked.Read(ref counters.PacketsIn),
                Interlocked.Read(ref counters.PacketsOut),
                Interlocked.Read(ref counters.BytesIn),
                Interlocked.Read(ref counters.BytesOut),
                dropSize,
                dropHeader,
                dropUnknownSsrc,
                Interlocked.Read(ref counters.DropRate),
                Interlocked.Read(ref counters.DropBadTag),
                Interlocked.Read(ref counters.DropReplay),
                Interlocked.Read(ref counters.DropNoAddress));
        }
    }

    private void RaiseSpeakingChanged(VoiceSession session, bool speaking)
    {
        var handler = SpeakingChanged;
        if (handler is null)
        {
            return;
        }

        try
        {
            handler(session.RoomId, session.UserId, speaking);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Voice speaking handler failed for user {UserId} in room {RoomId}", session.UserId, session.RoomId);
        }
    }

    private readonly record struct Inbound(VoiceSession Session, RoomCounters Counters, MediaHeader Header, int PlaintextLength);

    // Fields, not properties: Interlocked needs a ref to the storage.
    private sealed class RoomCounters
    {
        public long PacketsIn;
        public long PacketsOut;
        public long BytesIn;
        public long BytesOut;
        public long DropRate;
        public long DropBadTag;
        public long DropReplay;
        public long DropNoAddress;
    }
}
