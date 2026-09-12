using System.Buffers;
using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Collections.Immutable;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Threading.Channels;

namespace Vorcall.Server.Voice;

// The media path: one UDP socket, one receive loop, one key per voice session. Every datagram
// is authenticated before anything is forwarded, and the relay only ever sends to an address
// that a session's own key has already been proved from, so the socket cannot be used to
// bounce traffic at a third party. Screen share rides the same socket and the same key: its
// fan-out leaves the receive loop through a per-session queue so a sharer's watchers never cost
// the channel's audio a millisecond.
public sealed class VoiceRelay(VoiceOptions options, ILogger<VoiceRelay> logger) : BackgroundService
{
    private const int ReceiveBufferBytes = 8 * 1024 * 1024;
    private const int SendBufferBytes = 8 * 1024 * 1024;
    private const int DatagramBufferBytes = 2048;
    private const int MaxPlaintextBytes = MediaHeader.MaxDatagram - MediaHeader.MinDatagram;

    // A keyframe request carries the ssrc it is meant for, and nothing else.
    private const int KeyframeRequestBytes = sizeof(uint);

    private static readonly TimeSpan SpeakingTimeout = TimeSpan.FromMilliseconds(250);
    private static readonly TimeSpan HousekeepingInterval = TimeSpan.FromMilliseconds(100);
    private static readonly TimeSpan MetricsInterval = TimeSpan.FromSeconds(30);

    private readonly ConcurrentDictionary<uint, VoiceSession> _sessions = new();

    // Copy-on-write under _gate so the receive loop forwards over a snapshot without locking.
    private readonly ConcurrentDictionary<long, ImmutableArray<VoiceSession>> _channels = new();
    private readonly ConcurrentDictionary<long, ChannelCounters> _counters = new();
    private readonly Lock _gate = new();

    // These three drops happen before a datagram is attached to any session, so there is no
    // channel to charge them to: they are relay-wide and repeat in every channel's line.
    private long _dropSize;
    private long _dropHeader;
    private long _dropUnknownSsrc;

    private volatile Socket? _socket;

    // Raised from the receive loop and the housekeeping tick: handlers must be fast and must
    // not throw.
    public event Action<long, long, bool>? SpeakingChanged;

    public bool IsAvailable => _socket is not null;

    // Whether a share may be started at all: the kill switch plus a relay that actually came up.
    public bool ShareEnabled => options.ShareEnabled && IsAvailable;

    // How many sharers a channel may hold at once. The relay does not enforce it; the signalling
    // side does, and reads the ceiling from here.
    public int MaxSharersPerRoom => options.MaxSharersPerRoom;

    public string AdvertisedHost => options.Host;

    public int Port => options.Port;

    public VoiceSession CreateSession(long channelId, long userId)
        => CreateSession(channelId, userId, muted: false, deafened: false, priority: false);

    // The moderation flags belong to the session from its first packet: a server-muted member must
    // not be heard in the window between the join and a SetModeration call.
    public VoiceSession CreateSession(long channelId, long userId, bool muted, bool deafened, bool priority)
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

            session = new VoiceSession(ssrc, userId, channelId, key, options.ShareMaxKbps)
            {
                Muted = muted,
                Deafened = deafened,
                Priority = priority,
            };
            _sessions[ssrc] = session;
            _channels[channelId] = _channels.TryGetValue(channelId, out var members) ? members.Add(session) : [session];
        }

        logger.LogDebug("Voice session {Ssrc} opened for user {UserId} in channel {ChannelId}", session.Ssrc, userId, channelId);
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

            if (_channels.TryGetValue(session.ChannelId, out var members))
            {
                var remaining = members.Remove(session);
                if (remaining.IsEmpty)
                {
                    _channels.TryRemove(session.ChannelId, out _);
                }
                else
                {
                    _channels[session.ChannelId] = remaining;
                }
            }

            // Both directions of the share graph go with the session: whatever it was watching
            // loses a watcher, and its own watchers are left watching nothing, which is what
            // makes their next keyframe request a drop rather than a message to a stranger.
            StopWatching(session);
            DetachWatchers(session);
            session.Sharing = false;
            session.ShareAudio = false;
            session.CompleteShareQueue();
        }

        if (session.StopSpeaking())
        {
            RaiseSpeakingChanged(session, false);
        }

        session.Dispose();
        logger.LogDebug("Voice session {Ssrc} closed for user {UserId} in channel {ChannelId}", ssrc, session.UserId, session.ChannelId);
    }

    // Marks a session as sharing, or ends its share. Starting is idempotent: a second call only
    // updates whether the share carries audio. Ending detaches every watcher and stops the
    // sender worker. Raises no event and takes only the relay's own lock, so the signalling side
    // may call it while holding its own.
    public void SetSharing(uint ssrc, bool sharing, bool audio)
    {
        ChannelReader<VoiceSession.Outbound>? started = null;
        VoiceSession? session;

        lock (_gate)
        {
            if (!_sessions.TryGetValue(ssrc, out session))
            {
                return;
            }

            if (sharing)
            {
                session.ShareAudio = audio;
                if (!session.Sharing)
                {
                    // The queue comes first: a datagram may reach the receive loop the moment the
                    // flag is up, and it must find the queue this worker is reading.
                    started = session.RestartShareQueue();
                    session.Sharing = true;
                }
            }
            else
            {
                session.Sharing = false;
                session.ShareAudio = false;
                DetachWatchers(session);
                session.CompleteShareQueue();
            }
        }

        if (started is { } queue && session is { } sharer)
        {
            _ = Task.Run(() => ShareWorkerAsync(sharer, queue));
        }
    }

    // Applies the moderation flags to a live session. Takes only the relay's own lock and raises
    // nothing, so the signalling side may call it while holding its own: true says the mute ended a
    // talk spurt, which the caller announces once it has let go of that lock. An unknown ssrc is a
    // no-op, which is what a moderation frame racing a leave looks like from here.
    public bool SetModeration(uint ssrc, bool muted, bool deafened, bool priority)
    {
        lock (_gate)
        {
            if (!_sessions.TryGetValue(ssrc, out var session))
            {
                return false;
            }

            session.Muted = muted;
            session.Deafened = deafened;
            session.Priority = priority;

            // Once the audio is dropped nothing arrives to expire the talk spurt, so a mute that
            // lands mid-spurt has to end it or every client keeps showing a talker.
            return muted && session.StopSpeaking();
        }
    }

    // Points a viewer at one sharer, or at nobody. A target that is unknown, not sharing, or the
    // viewer itself only clears the previous watch.
    public void Watch(uint viewerSsrc, uint? sharerSsrc)
    {
        lock (_gate)
        {
            if (!_sessions.TryGetValue(viewerSsrc, out var viewer))
            {
                return;
            }

            StopWatching(viewer);

            if (sharerSsrc is not { } targetSsrc
                || targetSsrc == viewerSsrc
                || !_sessions.TryGetValue(targetSsrc, out var target)
                || !target.Sharing)
            {
                return;
            }

            target.Watchers = target.Watchers.Add(viewer);
            viewer.Watching = target;
        }
    }

    // How many viewers a share is being forwarded to. Zero for a session that is not sharing or
    // no longer exists.
    public int WatcherCount(uint ssrc)
    {
        lock (_gate)
        {
            return _sessions.TryGetValue(ssrc, out var session) ? session.Watchers.Length : 0;
        }
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
            socket.SendBufferSize = SendBufferBytes;
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

        // The kernel clamps the buffers to its own maximum without saying so, and a screen share
        // is what finds that ceiling, so the granted sizes are logged rather than the asked ones.
        logger.LogInformation(
            "Voice relay listening on 0.0.0.0:{Port} with socket buffers of {ReceiveBuffer} bytes in and {SendBuffer} bytes out",
            options.Port,
            socket.ReceiveBufferSize,
            socket.SendBufferSize);
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

    private static bool TrySeal(VoiceSession recipient, byte[] datagram, ReadOnlySpan<byte> plaintext)
    {
        var header = datagram.AsSpan(0, MediaHeader.Length);
        return recipient.TrySeal(
            MediaHeader.NonceOf(header),
            plaintext,
            datagram.AsSpan(MediaHeader.Length, plaintext.Length),
            datagram.AsSpan(MediaHeader.Length + plaintext.Length, MediaHeader.TagLength),
            header);
    }

    // Both callers hold _gate.
    private static void StopWatching(VoiceSession viewer)
    {
        if (viewer.Watching is { } current)
        {
            current.Watchers = current.Watchers.Remove(viewer);
            viewer.Watching = null;
        }
    }

    private static void DetachWatchers(VoiceSession sharer)
    {
        foreach (var watcher in sharer.Watchers)
        {
            watcher.Watching = null;
        }

        sharer.Watchers = [];
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

            switch (inbound.Header.Type)
            {
                case MediaHeader.TypeAudio:
                    if (inbound.Session.MarkAudio(Stopwatch.GetTimestamp()))
                    {
                        // The session was looked up before RemoveSession could have taken it out,
                        // so a blind raise here can land after the channel already saw
                        // VoiceMemberLeft and leave a talker nobody ever silences. A removed
                        // session simply goes quiet.
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
                    break;

                case MediaHeader.TypePing:
                    await PongAsync(socket, inbound, scratch, plaintext, stoppingToken);
                    break;

                case MediaHeader.TypeVideo:
                case MediaHeader.TypeShareAudio:
                    EnqueueShare(inbound, buffer, plaintext);
                    break;

                case MediaHeader.TypeKeyframeRequest:
                    await RequestKeyframeAsync(socket, inbound, scratch, plaintext, stoppingToken);
                    break;

                default:
                    // Unreachable: TryParse admits no other type.
                    break;
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

        var counters = _counters.GetOrAdd(session.ChannelId, static _ => new ChannelCounters());

        // Share media is a hundred packets a second on its own: it answers to a byte budget of
        // its own further down instead of the packet bucket audio and pings share.
        var share = header.Type is MediaHeader.TypeVideo or MediaHeader.TypeShareAudio;
        if (!share && !session.Bucket.TryTake(Stopwatch.GetTimestamp()))
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

        // Charged after the AEAD for the same reason the window moves after it: a forgery must
        // not be able to spend somebody else's budget.
        if (share)
        {
            if (!session.ShareBucket.TryTake(Stopwatch.GetTimestamp(), length))
            {
                Interlocked.Increment(ref counters.DropShareRate);
                return false;
            }

            if (!session.Sharing || (header.Type == MediaHeader.TypeShareAudio && !session.ShareAudio))
            {
                Interlocked.Increment(ref counters.DropNotSharing);
                return false;
            }
        }

        // Server mute, refused where the share flags are and for the same reason: a forgery must
        // not be able to probe the flag, and a muted sender must not mark itself speaking either.
        // Only audio is silenced — its pings, keyframe requests and share media carry on.
        if (header.Type == MediaHeader.TypeAudio && session.Muted)
        {
            Interlocked.Increment(ref counters.DropMuted);
            return false;
        }

        Interlocked.Increment(ref counters.PacketsIn);
        Interlocked.Add(ref counters.BytesIn, length);

        if (session.LearnAddress(address))
        {
            logger.LogDebug("Voice session of user {UserId} in channel {ChannelId} moved to another address", session.UserId, session.ChannelId);
        }

        inbound = new Inbound(session, counters, header, plaintextLength);
        return true;
    }

    // Selective forwarding: the header travels verbatim, the payload is re-sealed with each
    // recipient's own key. The sender never gets its own audio back.
    private async Task ForwardAsync(Socket socket, Inbound inbound, byte[] buffer, byte[] scratch, byte[] plaintext, CancellationToken stoppingToken)
    {
        if (!_channels.TryGetValue(inbound.Session.ChannelId, out var members))
        {
            return;
        }

        buffer.AsSpan(0, MediaHeader.Length).CopyTo(scratch);
        var length = MediaHeader.Length + inbound.PlaintextLength + MediaHeader.TagLength;
        var counters = inbound.Counters;

        foreach (var member in members)
        {
            // A deafened member is skipped as a recipient only: its own address and its pongs are
            // untouched.
            if (member.Ssrc == inbound.Session.Ssrc || member.Deafened)
            {
                continue;
            }

            if (member.Address is not { } destination)
            {
                Interlocked.Increment(ref counters.DropNoAddress);
                continue;
            }

            // A false seal means the member was removed while this packet was in flight.
            if (TrySeal(member, scratch, plaintext.AsSpan(0, inbound.PlaintextLength)))
            {
                await SendAsync(socket, scratch, length, destination, counters, stoppingToken);
            }
        }
    }

    // The receive loop's whole part in a share: one pooled copy of the header and its plaintext,
    // handed to the session's own worker. Everything after this happens off this thread.
    private void EnqueueShare(Inbound inbound, byte[] buffer, byte[] plaintext)
    {
        var counters = inbound.Counters;
        var length = MediaHeader.Length + inbound.PlaintextLength;
        var rented = ArrayPool<byte>.Shared.Rent(length);
        buffer.AsSpan(0, MediaHeader.Length).CopyTo(rented);
        plaintext.AsSpan(0, inbound.PlaintextLength).CopyTo(rented.AsSpan(MediaHeader.Length));

        // A full queue evicts its oldest frame instead, which the session counts itself; a
        // refusal here means the share ended while this datagram was in flight.
        if (!inbound.Session.ShareWriter.TryWrite(new VoiceSession.Outbound(rented, inbound.PlaintextLength)))
        {
            ArrayPool<byte>.Shared.Return(rented);
            Interlocked.Increment(ref counters.DropQueueFull);
            return;
        }

        Interlocked.Increment(ref counters.SharePacketsIn);
        Interlocked.Add(ref counters.ShareBytesIn, length + MediaHeader.TagLength);
    }

    // One sharer's fan-out, on its own task: the sends are synchronous because a share is a
    // burst of datagrams and nothing else waits on this thread.
    private async Task ShareWorkerAsync(VoiceSession session, ChannelReader<VoiceSession.Outbound> queue)
    {
        var scratch = new byte[MediaHeader.MaxDatagram];
        var counters = _counters.GetOrAdd(session.ChannelId, static _ => new ChannelCounters());

        try
        {
            await foreach (var item in queue.ReadAllAsync())
            {
                try
                {
                    if (!FanOut(session, item, scratch, counters))
                    {
                        return;
                    }
                }
                finally
                {
                    ArrayPool<byte>.Shared.Return(item.Buffer);
                }
            }
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Voice share worker for user {UserId} in channel {ChannelId} stopped", session.UserId, session.ChannelId);
        }
    }

    // False once the socket is gone, which is the worker's cue to leave.
    private bool FanOut(VoiceSession session, VoiceSession.Outbound item, byte[] scratch, ChannelCounters counters)
    {
        var socket = _socket;
        if (socket is null)
        {
            return false;
        }

        var watchers = session.Watchers;
        if (watchers.IsEmpty)
        {
            return true;
        }

        item.Buffer.AsSpan(0, MediaHeader.Length).CopyTo(scratch);
        var plaintext = item.Buffer.AsSpan(MediaHeader.Length, item.PlaintextLength);
        var length = MediaHeader.Length + item.PlaintextLength + MediaHeader.TagLength;

        foreach (var watcher in watchers)
        {
            if (watcher.Ssrc == session.Ssrc || watcher.Deafened)
            {
                continue;
            }

            // A watch is only ever set up inside one channel; a mismatch would be a bug in the
            // signalling side, and the frame is dropped rather than leaked to the other channel.
            if (watcher.ChannelId != session.ChannelId)
            {
                logger.LogDebug(
                    "Voice share of user {UserId} in channel {ChannelId} had a watcher from channel {WatcherChannelId}",
                    session.UserId,
                    session.ChannelId,
                    watcher.ChannelId);
                continue;
            }

            if (watcher.Address is not { } destination)
            {
                Interlocked.Increment(ref counters.DropNoAddress);
                continue;
            }

            if (!TrySeal(watcher, scratch, plaintext))
            {
                continue;
            }

            try
            {
                var sent = socket.SendTo(scratch, 0, length, SocketFlags.None, destination);
                Interlocked.Increment(ref counters.SharePacketsOut);
                Interlocked.Add(ref counters.ShareBytesOut, sent);
            }
            catch (SocketException ex)
            {
                logger.LogDebug(ex, "Voice share send failed with {SocketError}", ex.SocketErrorCode);
            }
            catch (ObjectDisposedException)
            {
                return false;
            }
        }

        return true;
    }

    // A viewer asking the share it is watching for a keyframe. The payload names the target, and
    // it has to be the one the relay knows this viewer is watching: nobody gets to poke a session
    // they are not receiving.
    private async Task RequestKeyframeAsync(Socket socket, Inbound inbound, byte[] scratch, byte[] plaintext, CancellationToken stoppingToken)
    {
        var counters = inbound.Counters;
        if (inbound.PlaintextLength < KeyframeRequestBytes
            || inbound.Session.Watching is not { } target
            || target.Ssrc != BinaryPrimitives.ReadUInt32BigEndian(plaintext))
        {
            Interlocked.Increment(ref counters.DropNotWatching);
            return;
        }

        if (target.Address is not { } destination)
        {
            Interlocked.Increment(ref counters.DropNoAddress);
            return;
        }

        inbound.Header.Write(scratch);
        if (TrySeal(target, scratch, plaintext.AsSpan(0, inbound.PlaintextLength)))
        {
            var length = MediaHeader.Length + inbound.PlaintextLength + MediaHeader.TagLength;
            await SendAsync(socket, scratch, length, destination, counters, stoppingToken);
            Interlocked.Increment(ref counters.KeyframeRequests);
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

        if (TrySeal(inbound.Session, scratch, plaintext.AsSpan(0, inbound.PlaintextLength)))
        {
            var length = MediaHeader.Length + inbound.PlaintextLength + MediaHeader.TagLength;
            await SendAsync(socket, scratch, length, destination, counters, stoppingToken);
        }
    }

    private async Task SendAsync(Socket socket, byte[] datagram, int length, IPEndPoint destination, ChannelCounters counters, CancellationToken stoppingToken)
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

        foreach (var (channelId, members) in _channels)
        {
            if (members.IsEmpty || !_counters.TryGetValue(channelId, out var counters))
            {
                continue;
            }

            // Evictions are counted on the session whose queue overflowed, so the channel's figure
            // is its own refusals plus what its sharers threw away.
            var queueDrops = Interlocked.Read(ref counters.DropQueueFull);
            foreach (var member in members)
            {
                queueDrops += member.QueueDrops;
            }

            logger.LogInformation(
                "Voice channel {ChannelId}: {Sessions} sessions, {PacketsIn} packets in, {PacketsOut} out, {BytesIn} bytes in, {BytesOut} out; share: {SharePacketsIn} packets in, {SharePacketsOut} out, {ShareBytesIn} bytes in, {ShareBytesOut} out, {KeyframeRequests} keyframe requests; drops: {DropSize} size, {DropHeader} header, {DropUnknownSsrc} unknown ssrc, {DropRate} rate, {DropBadTag} bad tag, {DropReplay} replay, {DropNoAddress} no address, {DropShareRate} share rate, {DropNotSharing} not sharing, {DropMuted} muted, {DropNotWatching} not watching, {DropQueueFull} queue full",
                channelId,
                members.Length,
                Interlocked.Read(ref counters.PacketsIn),
                Interlocked.Read(ref counters.PacketsOut),
                Interlocked.Read(ref counters.BytesIn),
                Interlocked.Read(ref counters.BytesOut),
                Interlocked.Read(ref counters.SharePacketsIn),
                Interlocked.Read(ref counters.SharePacketsOut),
                Interlocked.Read(ref counters.ShareBytesIn),
                Interlocked.Read(ref counters.ShareBytesOut),
                Interlocked.Read(ref counters.KeyframeRequests),
                dropSize,
                dropHeader,
                dropUnknownSsrc,
                Interlocked.Read(ref counters.DropRate),
                Interlocked.Read(ref counters.DropBadTag),
                Interlocked.Read(ref counters.DropReplay),
                Interlocked.Read(ref counters.DropNoAddress),
                Interlocked.Read(ref counters.DropShareRate),
                Interlocked.Read(ref counters.DropNotSharing),
                Interlocked.Read(ref counters.DropMuted),
                Interlocked.Read(ref counters.DropNotWatching),
                queueDrops);
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
            handler(session.ChannelId, session.UserId, speaking);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Voice speaking handler failed for user {UserId} in channel {ChannelId}", session.UserId, session.ChannelId);
        }
    }

    private readonly record struct Inbound(VoiceSession Session, ChannelCounters Counters, MediaHeader Header, int PlaintextLength);

    // Fields, not properties: Interlocked needs a ref to the storage.
    private sealed class ChannelCounters
    {
        public long PacketsIn;
        public long PacketsOut;
        public long BytesIn;
        public long BytesOut;
        public long SharePacketsIn;
        public long SharePacketsOut;
        public long ShareBytesIn;
        public long ShareBytesOut;
        public long KeyframeRequests;
        public long DropRate;
        public long DropBadTag;
        public long DropReplay;
        public long DropNoAddress;
        public long DropShareRate;
        public long DropNotSharing;
        public long DropMuted;
        public long DropNotWatching;
        public long DropQueueFull;
    }
}
