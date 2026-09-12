using System.Net.WebSockets;
using System.Text;
using Google.Protobuf;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;
using Xunit;
using Xunit.Sdk;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests.Infrastructure;

// System.Threading.Channels.Channel is qualified in full everywhere below, the way
// server/Chat/ClientConnection.cs does it: the protocol defines a Channel message too, and that
// is the Channel this suite talks about.

// Thrown by a read once the server has closed the socket: the code it closed with.
internal sealed class SocketClosedException(string label, int code) : Exception($"{label}: socket closed with {code}")
{
    public int Code { get; } = code;
}

// What a Hello handed back: the Welcome, the one ServerSnapshot carrying the whole world the
// reader may see, and one VoiceState per channel that had anybody in voice. Everything is keyed
// by the id the server assigned, because that is how every later frame names it; nothing is read
// positionally, the snapshot's own ordering being the server's business and not a test's.
internal sealed class Session(Welcome welcome, ServerSnapshot snapshot, IReadOnlyList<VoiceState> voiceStates)
{
    public Welcome Welcome { get; } = welcome;

    public ServerSnapshot Snapshot { get; } = snapshot;

    public Protocol.Server Server => Snapshot.Server;

    // The one text channel that can neither be deleted nor hidden. Named by the server row, never
    // by the name "general": channel names are not unique.
    public long GeneralId => Snapshot.Server.GeneralChannelId;

    public IReadOnlyDictionary<long, Channel> Channels { get; } = snapshot.Channels.ToDictionary(channel => channel.Id);

    public IReadOnlyDictionary<long, Role> Roles { get; } = snapshot.Roles.ToDictionary(role => role.Id);

    public IReadOnlyDictionary<long, Category> Categories { get; } = snapshot.Categories.ToDictionary(category => category.Id);

    public IReadOnlyDictionary<long, Profile> Members { get; } = snapshot.Members.ToDictionary(member => member.UserId);

    public IReadOnlyDictionary<long, ReadState> ReadStates { get; } = snapshot.ReadStates.ToDictionary(state => state.ChannelId);

    public IReadOnlyDictionary<long, VoiceState> VoiceStates { get; } = voiceStates.ToDictionary(state => state.ChannelId);

    public Channel General => Channel(GeneralId);

    // The voice channel Seed puts beside general, which is the voice and share tests' channel:
    // creating one of their own needs MANAGE_CHANNELS, and only the owner has that on a fresh
    // server.
    public long GeneralVoiceId => Named(ChannelKind.Voice, Data.Seed.GeneralVoiceChannelName);

    // The category Seed puts general in, for the tests that need an existing category id.
    public long GeneralCategoryId
    {
        get
        {
            var ids = Snapshot.Categories
                .Where(category => category.Name == Data.Seed.GeneralCategoryName)
                .Select(category => category.Id)
                .Order()
                .ToArray();
            Assert.True(ids.Length > 0, $"no category is named {Data.Seed.GeneralCategoryName}");
            return ids[0];
        }
    }

    // The everyone role: the undeletable one at position 0, found by its flag rather than its name.
    public Role Everyone
    {
        get
        {
            var everyone = Snapshot.Roles.Where(role => role.Everyone).ToArray();
            Assert.True(everyone.Length == 1, $"expected one everyone role, the snapshot has {everyone.Length}");
            return everyone[0];
        }
    }

    public Channel Channel(long channelId)
    {
        Assert.True(
            Channels.TryGetValue(channelId, out var channel),
            $"channel {channelId} is not in the snapshot ({string.Join(", ", Channels.Keys.Order())})");
        return channel!;
    }

    public Profile Member(long userId)
    {
        Assert.True(
            Members.TryGetValue(userId, out var member),
            $"member {userId} is not in the snapshot ({string.Join(", ", Members.Keys.Order())})");
        return member!;
    }

    public ReadState Read(long channelId)
    {
        Assert.True(
            ReadStates.TryGetValue(channelId, out var state),
            $"channel {channelId} has no read state ({string.Join(", ", ReadStates.Keys.Order())})");
        return state!;
    }

    public VoiceState Voice(long channelId)
    {
        Assert.True(
            VoiceStates.TryGetValue(channelId, out var state),
            $"channel {channelId} had no VoiceState at hello ({string.Join(", ", VoiceStates.Keys.Order())})");
        return state!;
    }

    private long Named(ChannelKind kind, string name)
    {
        var ids = Snapshot.Channels
            .Where(channel => channel.Kind == kind && channel.Name == name)
            .Select(channel => channel.Id)
            .Order()
            .ToArray();
        Assert.True(ids.Length > 0, $"no {kind} channel is named {name}");
        return ids[0];
    }
}

// One client socket. A pump task reads every frame into an inbox as it arrives, so a read that
// times out cancels only the wait on the inbox, never a receive on the socket itself.
internal sealed class WsClient : IAsyncDisposable
{
    public static readonly TimeSpan DefaultTimeout = TimeSpan.FromSeconds(5);
    public static readonly TimeSpan QuietWindow = TimeSpan.FromSeconds(1);

    // Screen share (PROTOCOL.md "Screen share") signals on the voice session, so its frames are
    // optional to a client in exactly the same way voice frames are.
    public static readonly IReadOnlySet<Kind> ShareKinds = new HashSet<Kind>
    {
        Kind.ShareStarted, Kind.ShareStopped, Kind.WatchState, Kind.ShareWatchers,
    };

    // Voice frames (PROTOCOL.md "Voice") ride the same socket; a client without voice support
    // ignores them, and so does a read here unless it asks for one.
    public static readonly IReadOnlySet<Kind> VoiceKinds = new HashSet<Kind>(ShareKinds)
    {
        Kind.VoiceReady, Kind.VoiceState, Kind.VoiceMemberJoined, Kind.VoiceMemberLeft, Kind.Speaking, Kind.VoiceMoved,
    };

    private static readonly IReadOnlySet<Kind> Nothing = new HashSet<Kind>();

    private readonly WebSocket _socket;
    private readonly System.Threading.Channels.Channel<ServerFrame> _inbox =
        System.Threading.Channels.Channel.CreateUnbounded<ServerFrame>(
            new System.Threading.Channels.UnboundedChannelOptions { SingleReader = true, SingleWriter = true });
    private readonly TaskCompletionSource<int> _closed = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly Task _pump;
    private Session? _session;

    private WsClient(WebSocket socket, string label)
    {
        _socket = socket;
        Label = label;
        _pump = Task.Run(PumpAsync);
    }

    public string Label { get; }

    // What the Hello sequence handed back; a socket that never said hello has none.
    public Session Session => _session ?? throw new InvalidOperationException($"{Label}: no hello sequence was read");

    // The upgrade alone. A refused upgrade surfaces as the InvalidOperationException the test
    // host raises, whose message names the status code.
    public static async Task<WsClient> OpenAsync(VorcallFactory factory, string? bearer, string label, string? key = ServerFixture.ServerKey)
    {
        var client = factory.Server.CreateWebSocketClient();
        client.ConfigureRequest = request =>
        {
            if (key is not null)
            {
                request.Headers[ServerKeyMiddleware.HeaderName] = key;
            }

            if (bearer is not null)
            {
                request.Headers.Authorization = $"Bearer {bearer}";
            }
        };

        var socket = await client.ConnectAsync(new Uri(factory.Server.BaseAddress, "/ws"), CancellationToken.None);
        return new WsClient(socket, label);
    }

    // Opens the socket, says hello and consumes the whole Hello sequence.
    public static async Task<WsClient> ConnectAsync(VorcallFactory factory, Account account, string? label = null)
    {
        var client = await OpenAsync(factory, account.Access, label ?? account.Username);
        try
        {
            await client.HelloAsync(account);
            return client;
        }
        catch
        {
            await client.DisposeAsync();
            throw;
        }
    }

    // Replaces a live connection: the old socket dies fatally, the new one re-reads the server.
    public static async Task<WsClient> ReconnectAsync(VorcallFactory factory, Account account, WsClient old, string? label = null)
    {
        var client = await ConnectAsync(factory, account, label);
        await old.ExpectErrorAsync(ErrorCode.SessionReplaced, fatal: true);
        await old.ExpectClosedAsync(1008);
        return client;
    }

    public async Task<Session> HelloAsync(Account account, uint protocolVersion = 1)
    {
        await SendAsync(Frames.Hello(protocolVersion));
        return await ReadHelloAsync(account);
    }

    // Welcome, the one ServerSnapshot, then one VoiceState per channel that has anybody in voice.
    // Nothing terminates that run on the wire, so a Ping is sent ahead of the read and its Pong is
    // the fence: one connection's frames are answered in order, so a Pong proves the sequence is
    // complete and that nothing else arrived inside it.
    public async Task<Session> ReadHelloAsync(Account account)
    {
        var marker = Frames.NowMs();
        await SendAsync(Frames.Ping(marker));

        var first = await ReceiveStrictAsync();
        Assert.True(first.PayloadCase == Kind.Welcome, $"{Label}: expected Welcome, got {Describe(first)}");
        var welcome = first.Welcome;
        Assert.True(welcome.MemberId == account.UserId, $"{Label}: Welcome names member {welcome.MemberId}, not {account.UserId}");
        Assert.Equal(account.Username, welcome.Username);

        var second = await ReceiveStrictAsync();
        Assert.True(second.PayloadCase == Kind.ServerSnapshot, $"{Label}: expected ServerSnapshot, got {Describe(second)}");
        var snapshot = second.ServerSnapshot;

        var voiceStates = new List<VoiceState>();
        while (true)
        {
            var frame = await ReceiveStrictAsync();
            if (frame.PayloadCase == Kind.VoiceState)
            {
                voiceStates.Add(frame.VoiceState);
                continue;
            }

            Assert.True(frame.PayloadCase == Kind.Pong, $"{Label}: expected VoiceState or Pong, got {Describe(frame)}");
            Assert.Equal(marker, frame.Pong.SentAtUnixMs);
            break;
        }

        var session = new Session(welcome, snapshot, voiceStates);
        Assert.True(
            snapshot.Server.GeneralChannelId != 0 && session.Channels.ContainsKey(snapshot.Server.GeneralChannelId),
            $"{Label}: general ({snapshot.Server.GeneralChannelId}) is missing from the snapshot");
        Assert.True(session.Members.ContainsKey(account.UserId), $"{Label}: the reader is not among the snapshot's members");
        _session = session;
        return session;
    }

    public Task SendAsync(ClientFrame frame) => SendBytesAsync(frame.ToByteArray());

    public Task SendBytesAsync(byte[] payload)
        => _socket.SendAsync(new ArraySegment<byte>(payload), WebSocketMessageType.Binary, endOfMessage: true, CancellationToken.None);

    public Task SendTextAsync(string text)
        => _socket.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes(text)), WebSocketMessageType.Text, endOfMessage: true, CancellationToken.None);

    // The next frame that is not an unrequested voice frame. Times out with a TimeoutException,
    // and reports a closed socket as a SocketClosedException.
    public async Task<ServerFrame> ReceiveAsync(TimeSpan? timeout = null, IReadOnlySet<Kind>? keep = null)
    {
        var kept = keep ?? Nothing;
        var window = timeout ?? DefaultTimeout;
        using var cts = new CancellationTokenSource(window);
        while (true)
        {
            ServerFrame frame;
            try
            {
                frame = await _inbox.Reader.ReadAsync(cts.Token);
            }
            catch (OperationCanceledException)
            {
                throw new TimeoutException($"{Label}: no frame within {window.TotalSeconds:0.#}s");
            }
            catch (System.Threading.Channels.ChannelClosedException)
            {
                throw new SocketClosedException(Label, await _closed.Task);
            }

            if (VoiceKinds.Contains(frame.PayloadCase) && !kept.Contains(frame.PayloadCase))
            {
                continue;
            }

            return frame;
        }
    }

    // The next frame whatever it is: voice and share frames included, nothing skipped.
    public Task<ServerFrame> ReceiveStrictAsync(TimeSpan? timeout = null) => ReceiveAsync(timeout, VoiceKinds);

    public async Task<ServerFrame> ExpectAsync(Kind kind, TimeSpan? timeout = null)
    {
        var frame = await ReceiveAsync(timeout, new HashSet<Kind> { kind });
        Assert.True(frame.PayloadCase == kind, $"{Label}: expected {kind}, got {Describe(frame)}");
        return frame;
    }

    // The frames of `kinds` in that exact order, with none of them skipped on the way: every
    // kind is kept for the whole read, so one arriving out of order fails the assertion instead
    // of being dropped as an unwanted voice frame.
    public async Task<ServerFrame[]> ExpectSequenceAsync(params Kind[] kinds)
    {
        var keep = new HashSet<Kind>(kinds);
        var frames = new ServerFrame[kinds.Length];
        for (var i = 0; i < kinds.Length; i++)
        {
            var frame = await ReceiveAsync(null, keep);
            Assert.True(frame.PayloadCase == kinds[i], $"{Label}: expected {kinds[i]} at position {i}, got {Describe(frame)}");
            frames[i] = frame;
        }

        return frames;
    }

    public async Task<Error> ExpectErrorAsync(ErrorCode code, bool fatal, TimeSpan? timeout = null)
    {
        var error = (await ExpectAsync(Kind.Error, timeout)).Error;
        Assert.True(error.Code == code, $"{Label}: expected {code}, got {error.Code} ({error.Detail})");
        Assert.True(error.Fatal == fatal, $"{Label}: expected fatal={fatal}, got fatal={error.Fatal} ({error.Detail})");
        return error;
    }

    // Presence, roles, nickname, profile and the server mute flags all arrive as MemberUpdated, so
    // the caller says which member it is waiting for and, when it is presence, which way.
    public async Task<Profile> ExpectMemberUpdatedAsync(Account member, bool? online = null, TimeSpan? timeout = null)
    {
        var updated = (await ExpectAsync(Kind.MemberUpdated, timeout)).MemberUpdated.Member;
        Assert.Equal(member.UserId, updated.UserId);
        Assert.Equal(member.Username, updated.Username);
        if (online is { } expected)
        {
            Assert.True(updated.Online == expected, $"{Label}: {member} is online={updated.Online}, expected {expected}");
        }

        return updated;
    }

    public async Task<Channel> ExpectChannelUpsertedAsync(long channelId, TimeSpan? timeout = null)
    {
        var channel = (await ExpectAsync(Kind.ChannelUpserted, timeout)).ChannelUpserted.Channel;
        Assert.Equal(channelId, channel.Id);
        return channel;
    }

    // A channel deleted for everybody, and a channel one member has lost sight of, are the same
    // frame addressed differently.
    public async Task ExpectChannelDeletedAsync(long channelId, TimeSpan? timeout = null)
    {
        var deleted = (await ExpectAsync(Kind.ChannelDeleted, timeout)).ChannelDeleted;
        Assert.Equal(channelId, deleted.Id);
    }

    public async Task ExpectMemberRemovedAsync(Account member, TimeSpan? timeout = null)
    {
        var removed = (await ExpectAsync(Kind.MemberRemoved, timeout)).MemberRemoved;
        Assert.Equal(member.UserId, removed.UserId);
    }

    // A frame sitting in the inbox is a frame the server sent before it closed: a message where
    // a close was expected fails.
    public async Task<int> ExpectClosedAsync(int? code = null, TimeSpan? timeout = null)
    {
        ServerFrame frame;
        try
        {
            frame = await ReceiveAsync(timeout);
        }
        catch (SocketClosedException closed)
        {
            if (code is { } expected)
            {
                Assert.True(expected == closed.Code, $"{Label}: expected close {expected}, got {closed.Code}");
            }

            return closed.Code;
        }

        throw new XunitException($"{Label}: socket not closed; received {Describe(frame)}");
    }

    public async Task QuietAsync(IReadOnlySet<Kind>? keep = null, TimeSpan? window = null)
    {
        ServerFrame frame;
        try
        {
            frame = await ReceiveAsync(window ?? QuietWindow, keep);
        }
        catch (TimeoutException)
        {
            return;
        }

        throw new XunitException($"{Label}: expected silence, received {Describe(frame)}");
    }

    // Frames of one connection are handled in order, so a pong proves everything sent before
    // the ping has been acted on, including frames that have no reply of their own.
    public async Task PingFenceAsync(long marker)
    {
        await SendAsync(Frames.Ping(marker));
        var pong = (await ExpectAsync(Kind.Pong)).Pong;
        Assert.Equal(marker, pong.SentAtUnixMs);
    }

    // The client half of the close handshake; the server mirrors it, and the pump sees that.
    public async Task CloseAsync()
    {
        if (_socket.State == WebSocketState.Open)
        {
            await _socket.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
        }

        await Task.WhenAny(_pump, Task.Delay(DefaultTimeout));
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            await CloseAsync();
        }
        catch (Exception ex) when (ex is WebSocketException or ObjectDisposedException or InvalidOperationException)
        {
            // Already torn down by the server, or by an earlier close: nothing left to hand back.
        }

        _socket.Dispose();
    }

    // Never the whole frame for VoiceReady: it carries the session's media key.
    public static string Describe(ServerFrame frame) => frame.PayloadCase switch
    {
        Kind.VoiceReady => $"VoiceReady{{channel={frame.VoiceReady.ChannelId}, ssrc={frame.VoiceReady.Ssrc}}}",
        _ => $"{frame.PayloadCase} {frame}",
    };

    private async Task PumpAsync()
    {
        var buffer = new byte[16 * 1024];
        using var assembled = new MemoryStream();
        try
        {
            while (true)
            {
                var result = await _socket.ReceiveAsync(new ArraySegment<byte>(buffer), CancellationToken.None);
                if (result.MessageType == WebSocketMessageType.Close)
                {
                    _closed.TrySetResult((int?)result.CloseStatus ?? 1005);
                    _inbox.Writer.TryComplete();

                    // Answering the server's close is what lets it finish the handshake at once
                    // instead of waiting out its drain timeout; a server that has already torn
                    // the socket down has nothing left to answer.
                    if (_socket.State == WebSocketState.CloseReceived)
                    {
                        try
                        {
                            await _socket.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, null, CancellationToken.None);
                        }
                        catch (Exception ex) when (ex is WebSocketException or IOException or ObjectDisposedException or InvalidOperationException)
                        {
                        }
                    }

                    return;
                }

                assembled.Write(buffer, 0, result.Count);
                if (!result.EndOfMessage)
                {
                    continue;
                }

                _inbox.Writer.TryWrite(ServerFrame.Parser.ParseFrom(assembled.ToArray()));
                assembled.SetLength(0);
            }
        }
        catch (Exception ex)
        {
            _closed.TrySetException(ex);
            _inbox.Writer.TryComplete(ex);
        }
    }
}
