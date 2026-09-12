using System.Net.WebSockets;
using System.Text;
using System.Threading.Channels;
using Google.Protobuf;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;
using Xunit;
using Xunit.Sdk;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests.Infrastructure;

// Thrown by a read once the server has closed the socket: the code it closed with.
internal sealed class SocketClosedException(string label, int code) : Exception($"{label}: socket closed with {code}")
{
    public int Code { get; } = code;
}

// What a Hello handed back: one RoomState per room the account is in, plus the RoomList. Both
// are keyed by room id rather than read positionally: PROTOCOL.md fixes general as the first
// RoomState, but the RoomList is every public room plus the account's DMs, in no order a test
// depends on.
internal sealed class Session(Welcome welcome, IReadOnlyList<RoomState> states, RoomList list)
{
    public const string General = "general";

    public Welcome Welcome { get; } = welcome;

    public IReadOnlyDictionary<string, RoomState> States { get; } = states.ToDictionary(state => state.RoomId);

    public IReadOnlyDictionary<string, RoomEntry> Rooms { get; } = list.Rooms.ToDictionary(entry => entry.Room.RoomId);

    public RoomState GeneralState => States[General];

    public RoomEntry Entry(string roomId)
    {
        Assert.True(Rooms.TryGetValue(roomId, out var entry), $"{roomId} is not in the RoomList ({string.Join(", ", Rooms.Keys.Order())})");
        return entry!;
    }

    public bool Joined(string roomId, long userId) => Entry(roomId).Room.MemberIds.Contains(userId);

    public static Dictionary<long, string> MembersOf(RoomState state) => state.Members.ToDictionary(member => member.UserId, member => member.Username);
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
    private static readonly IReadOnlySet<Kind> VoiceKinds = new HashSet<Kind>(ShareKinds)
    {
        Kind.VoiceReady, Kind.VoiceState, Kind.VoiceMemberJoined, Kind.VoiceMemberLeft, Kind.Speaking,
    };

    private static readonly IReadOnlySet<Kind> Nothing = new HashSet<Kind>();

    private readonly WebSocket _socket;
    private readonly Channel<ServerFrame> _inbox = Channel.CreateUnbounded<ServerFrame>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = true });
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

    // Replaces a live connection: the old socket dies fatally, the new one re-reads its rooms.
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

    // Welcome, every RoomState (VoiceState rides along and is skipped) and the RoomList.
    public async Task<Session> ReadHelloAsync(Account account)
    {
        var welcome = (await ExpectAsync(Kind.Welcome)).Welcome;
        Assert.True(welcome.MemberId == account.UserId, $"{Label}: Welcome names member {welcome.MemberId}, not {account.UserId}");
        Assert.Equal(account.Username, welcome.Username);

        var states = new List<RoomState>();
        RoomList list;
        while (true)
        {
            var frame = await ReceiveAsync();
            if (frame.PayloadCase == Kind.RoomState)
            {
                states.Add(frame.RoomState);
                continue;
            }

            Assert.True(frame.PayloadCase == Kind.RoomList, $"{Label}: expected RoomState or RoomList, got {Describe(frame)}");
            list = frame.RoomList;
            break;
        }

        // PROTOCOL.md fixes general as the first RoomState of the sequence.
        Assert.True(states.Count > 0 && states[0].RoomId == Session.General, $"{Label}: general is not the first RoomState");
        var session = new Session(welcome, states, list);
        Assert.True(session.Rooms.ContainsKey(Session.General), $"{Label}: general is missing from the RoomList");
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
            catch (ChannelClosedException)
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

    public async Task<MemberJoined> ExpectMemberJoinedAsync(string roomId, Account member)
    {
        var joined = (await ExpectAsync(Kind.MemberJoined)).MemberJoined;
        Assert.Equal(roomId, joined.RoomId);
        Assert.Equal(member.UserId, joined.Member.UserId);
        Assert.Equal(member.Username, joined.Member.Username);
        return joined;
    }

    public async Task<MemberLeft> ExpectMemberLeftAsync(string roomId, Account member)
    {
        var left = (await ExpectAsync(Kind.MemberLeft)).MemberLeft;
        Assert.Equal(roomId, left.RoomId);
        Assert.Equal(member.UserId, left.UserId);
        return left;
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
        Kind.VoiceReady => $"VoiceReady{{room={frame.VoiceReady.RoomId}, ssrc={frame.VoiceReady.Ssrc}}}",
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
