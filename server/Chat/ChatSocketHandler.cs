using System.Net.WebSockets;
using Google.Protobuf;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

// Implements the per-connection session state machine of PROTOCOL.md: AwaitingHello with a
// 5 s deadline, then Ready with a 120 s idle deadline.
public sealed class ChatSocketHandler(
    ConnectionRegistry registry,
    MessageService messages,
    IHostApplicationLifetime lifetime,
    ILogger<ChatSocketHandler> logger)
{
    private const int MaxInboundBytes = 16 * 1024;
    private const int ReceiveBufferSize = 4 * 1024;
    private const uint SupportedProtocolVersion = 1;

    // 1008 for every protocol violation, 1001 for idle and shutdown.
    private const WebSocketCloseStatus ProtocolClose = WebSocketCloseStatus.PolicyViolation;
    private const WebSocketCloseStatus GoingAwayClose = WebSocketCloseStatus.EndpointUnavailable;

    // Shared with Program's shutdown hook so both close paths report the same reason.
    public const string ShutdownReason = "server shutting down";

    private static readonly TimeSpan HelloDeadline = TimeSpan.FromSeconds(5);
    private static readonly TimeSpan IdleDeadline = TimeSpan.FromSeconds(120);

    public async Task HandleAsync(WebSocket socket, CancellationToken requestAborted)
    {
        var connection = new ClientConnection(socket, logger, requestAborted, lifetime.ApplicationStopping);
        var reader = new SocketReader(connection, logger);
        registry.Add(connection);
        logger.LogDebug("Connection {ConnectionId} accepted", connection.Id);

        try
        {
            if (await HandshakeAsync(connection, reader))
            {
                await PumpAsync(connection, reader);
            }
        }
        catch (Exception ex) when (ex is WebSocketException or OperationCanceledException)
        {
            logger.LogDebug(ex, "Connection {ConnectionId} ({Nickname}) torn down", connection.Id, connection.Nickname);

            // The host stopping cancels this connection's lifetime, so the receive loop can
            // reach here before ConnectionRegistry.CloseAllAsync does. Both paths must close
            // with the same 1001 shutdown reason, whichever of them latches the close first.
            await (lifetime.ApplicationStopping.IsCancellationRequested
                ? CloseAsync(connection, GoingAwayClose, ShutdownReason)
                : CloseAsync(connection, GoingAwayClose, "connection lost"));
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Connection {ConnectionId} ({Nickname}) failed unexpectedly", connection.Id, connection.Nickname);
            await CloseAsync(connection, WebSocketCloseStatus.InternalServerError, "internal error");
        }
        finally
        {
            registry.Remove(connection);
            await reader.DrainAsync();
            connection.Dispose();
        }
    }

    private async Task<bool> HandshakeAsync(ClientConnection connection, SocketReader reader)
    {
        var received = await reader.ReadAsync(HelloDeadline);
        switch (received.Outcome)
        {
            case ReceiveOutcome.ClientClose:
                await MirrorCloseAsync(connection, received);
                return false;
            case ReceiveOutcome.Closed:
                logger.LogDebug("Connection {ConnectionId} stopped receiving: close already latched", connection.Id);
                LogDisconnect(connection);
                return false;
            case ReceiveOutcome.Timeout:
                await FailAsync(connection, ErrorCode.Protocol, "no hello within 5s", ProtocolClose, "hello timeout");
                return false;
            case ReceiveOutcome.TooLarge:
                await FailAsync(connection, ErrorCode.Protocol, "message exceeds 16384 bytes", ProtocolClose, "message too large");
                return false;
            case ReceiveOutcome.TextFrame:
                await FailAsync(connection, ErrorCode.Protocol, "text frames are not accepted", ProtocolClose, "text frame");
                return false;
        }

        if (!TryParse(connection, received.Payload, out var frame))
        {
            await FailAsync(connection, ErrorCode.Protocol, "unparsable frame", ProtocolClose, "unparsable frame");
            return false;
        }

        if (frame.PayloadCase != ClientFrame.PayloadOneofCase.Hello)
        {
            await FailAsync(connection, ErrorCode.Protocol, "the first frame must be hello", ProtocolClose, "hello expected");
            return false;
        }

        if (frame.Hello.ProtocolVersion != SupportedProtocolVersion)
        {
            await FailAsync(connection, ErrorCode.Protocol, "unsupported protocol version", ProtocolClose, "unsupported version");
            return false;
        }

        if (!Validation.TryNormalizeNickname(frame.Hello.Nickname, out var nickname))
        {
            await FailAsync(connection, ErrorCode.InvalidNickname, "nickname must be 1..32 characters without control characters", ProtocolClose, "invalid nickname");
            return false;
        }

        var latestMessageId = await messages.GetLatestIdAsync();

        // Welcome is queued before the connection joins the broadcast set, so no ChatMessage
        // can ever overtake it.
        connection.TryEnqueue(new ServerFrame { Welcome = new Welcome { LatestMessageId = latestMessageId } });
        connection.MarkReady(nickname);
        logger.LogInformation(
            "Connection {ConnectionId} ({Nickname}) connected; {ConnectionCount} live, latest message {LatestMessageId}",
            connection.Id,
            nickname,
            registry.Count,
            latestMessageId);
        return true;
    }

    private async Task PumpAsync(ClientConnection connection, SocketReader reader)
    {
        while (true)
        {
            var received = await reader.ReadAsync(IdleDeadline);
            switch (received.Outcome)
            {
                case ReceiveOutcome.ClientClose:
                    await MirrorCloseAsync(connection, received);
                    return;
                case ReceiveOutcome.Closed:
                    logger.LogDebug(
                        "Connection {ConnectionId} ({Nickname}) stopped receiving: close already latched",
                        connection.Id,
                        connection.Nickname);
                    LogDisconnect(connection);
                    return;
                case ReceiveOutcome.Timeout:
                    await CloseAsync(connection, GoingAwayClose, "idle");
                    return;
                case ReceiveOutcome.TooLarge:
                    await FailAsync(connection, ErrorCode.Protocol, "message exceeds 16384 bytes", ProtocolClose, "message too large");
                    return;
                case ReceiveOutcome.TextFrame:
                    await FailAsync(connection, ErrorCode.Protocol, "text frames are not accepted", ProtocolClose, "text frame");
                    return;
            }

            if (!TryParse(connection, received.Payload, out var frame))
            {
                await FailAsync(connection, ErrorCode.Protocol, "unparsable frame", ProtocolClose, "unparsable frame");
                return;
            }

            switch (frame.PayloadCase)
            {
                case ClientFrame.PayloadOneofCase.Send:
                    if (!await HandleSendAsync(connection, frame.Send))
                    {
                        await CloseAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
                        return;
                    }

                    break;

                case ClientFrame.PayloadOneofCase.Ping:
                    if (!connection.TryEnqueue(new ServerFrame { Pong = new Pong { SentAtUnixMs = frame.Ping.SentAtUnixMs } }))
                    {
                        await CloseAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
                        return;
                    }

                    break;

                case ClientFrame.PayloadOneofCase.Hello:
                    await FailAsync(connection, ErrorCode.Protocol, "hello was already received", ProtocolClose, "duplicate hello");
                    return;

                default:
                    await FailAsync(connection, ErrorCode.Protocol, "frame carries no payload", ProtocolClose, "empty payload");
                    return;
            }
        }
    }

    // False when the sender's own outbox is full, which makes it a slow consumer.
    private async Task<bool> HandleSendAsync(ClientConnection connection, SendMessage send)
    {
        // A latched close must never persist or broadcast another message, however this frame
        // was read: the close wins even if it landed mid-receive.
        if (!connection.IsReady)
        {
            logger.LogDebug(
                "Connection {ConnectionId} ({Nickname}) sent after close was latched; dropping",
                connection.Id,
                connection.Nickname);
            return true;
        }

        if (!Validation.TryNormalizeText(send.Text, out var text))
        {
            logger.LogWarning("Connection {ConnectionId} ({Nickname}) sent invalid text", connection.Id, connection.Nickname);
            return connection.TryEnqueue(new ServerFrame
            {
                Error = new Error
                {
                    Code = ErrorCode.InvalidMessage,
                    Detail = "text must be 1..2000 characters after trimming",
                    Fatal = false,
                },
            });
        }

        // Persist first: an id only exists once the row is committed, and the broadcast
        // carries that id.
        var chatMessage = await messages.AppendAsync(connection.Nickname!, text);
        registry.Broadcast(new ServerFrame { Message = chatMessage });
        return true;
    }

    // A pending WebSocket receive cannot be cancelled without aborting the socket, which would
    // destroy the connection before the fatal error and close frames could be written. Deadlines
    // are therefore enforced by racing the receive against a timer and leaving it pending; the
    // same pending receive later picks up the peer's answer to our close frame.
    private sealed class SocketReader(ClientConnection connection, ILogger logger)
    {
        private static readonly TimeSpan DrainTimeout = TimeSpan.FromSeconds(2);

        private readonly byte[] _buffer = new byte[ReceiveBufferSize];
        private Task<WebSocketReceiveResult>? _pending;

        public async Task<ReceiveResult> ReadAsync(TimeSpan deadline)
        {
            var deadlineAt = Environment.TickCount64 + (long)deadline.TotalMilliseconds;
            using var assembled = new MemoryStream();

            while (true)
            {
                var receive = _pending ??= connection.Socket.ReceiveAsync(new ArraySegment<byte>(_buffer), CancellationToken.None);
                if (!await CompletesInTimeAsync(receive, deadlineAt))
                {
                    connection.Lifetime.ThrowIfCancellationRequested();
                    if (connection.Closing.IsCancellationRequested)
                    {
                        return ReceiveResult.Signal(ReceiveOutcome.Closed);
                    }

                    return ReceiveResult.Signal(ReceiveOutcome.Timeout);
                }

                _pending = null;
                var result = await receive;

                switch (result.MessageType)
                {
                    case WebSocketMessageType.Close:
                        return ReceiveResult.ClientClose(
                            result.CloseStatus ?? WebSocketCloseStatus.NormalClosure,
                            result.CloseStatusDescription ?? string.Empty);
                    case WebSocketMessageType.Text:
                        return ReceiveResult.Signal(ReceiveOutcome.TextFrame);
                }

                if (assembled.Length + result.Count > MaxInboundBytes)
                {
                    return ReceiveResult.Signal(ReceiveOutcome.TooLarge);
                }

                assembled.Write(_buffer, 0, result.Count);
                if (result.EndOfMessage)
                {
                    return ReceiveResult.Frame(assembled.ToArray());
                }
            }
        }

        // Observes the receive left pending by a deadline, so it is never an unobserved task.
        // A peer that answered our close frame completes it at once; one that went quiet is
        // cut off instead of holding the request open.
        public async Task DrainAsync()
        {
            var pending = _pending;
            if (pending is null)
            {
                return;
            }

            _pending = null;
            try
            {
                if (await Task.WhenAny(pending, Task.Delay(DrainTimeout)) != pending)
                {
                    connection.Socket.Abort();
                }

                await pending;
            }
            catch (Exception ex) when (ex is WebSocketException or OperationCanceledException or ObjectDisposedException)
            {
                logger.LogDebug(ex, "Connection {ConnectionId}: pending receive ended", connection.Id);
            }
            catch (Exception ex)
            {
                logger.LogError(ex, "Connection {ConnectionId}: pending receive failed unexpectedly", connection.Id);
            }
        }

        private async Task<bool> CompletesInTimeAsync(Task receive, long deadlineAt)
        {
            var remaining = deadlineAt - Environment.TickCount64;
            if (remaining <= 0)
            {
                return false;
            }

            // The timer is cancelled as soon as the receive wins so it does not linger.
            using var timer = CancellationTokenSource.CreateLinkedTokenSource(connection.Lifetime, connection.Closing);
            var finished = await Task.WhenAny(receive, Task.Delay(TimeSpan.FromMilliseconds(remaining), timer.Token));
            timer.Cancel();
            return finished == receive;
        }
    }

    private bool TryParse(ClientConnection connection, byte[] payload, out ClientFrame frame)
    {
        try
        {
            frame = ClientFrame.Parser.ParseFrom(payload);
            return true;
        }
        catch (InvalidProtocolBufferException ex)
        {
            logger.LogWarning(ex, "Connection {ConnectionId} ({Nickname}) sent an unparsable frame", connection.Id, connection.Nickname);
            frame = new ClientFrame();
            return false;
        }
    }

    private async Task MirrorCloseAsync(ClientConnection connection, ReceiveResult received)
        => await CloseAsync(connection, received.ClientStatus, received.ClientReason);

    private async Task FailAsync(ClientConnection connection, ErrorCode code, string detail, WebSocketCloseStatus status, string reason)
    {
        logger.LogWarning(
            "Connection {ConnectionId} ({Nickname}) protocol error {ErrorCode}: {Detail}",
            connection.Id,
            connection.Nickname,
            code,
            detail);
        await connection.FailAsync(new Error { Code = code, Detail = detail, Fatal = true }, status, reason);
        LogDisconnect(connection, status, reason);
    }

    private async Task CloseAsync(ClientConnection connection, WebSocketCloseStatus status, string reason)
    {
        await connection.CloseAsync(status, reason);
        LogDisconnect(connection, status, reason);
    }

    // The latched pair, not the requested one: CloseAsync is idempotent, so an earlier caller
    // (a 1013 from the registry, say) may already have decided how this connection ends. The
    // requested pair is only a fallback, and a close this handler awaited always latched one.
    private void LogDisconnect(ClientConnection connection, WebSocketCloseStatus? requestedStatus = null, string? requestedReason = null)
        => logger.LogInformation(
            "Connection {ConnectionId} ({Nickname}) disconnected with {CloseCode} {CloseReason}",
            connection.Id,
            connection.Nickname,
            (int)(connection.CloseStatus ?? requestedStatus ?? WebSocketCloseStatus.Empty),
            connection.CloseReason ?? requestedReason ?? string.Empty);

    private enum ReceiveOutcome
    {
        Frame,
        ClientClose,
        Closed,
        Timeout,
        TooLarge,
        TextFrame,
    }

    private readonly record struct ReceiveResult(
        ReceiveOutcome Outcome,
        byte[] Payload,
        WebSocketCloseStatus ClientStatus,
        string ClientReason)
    {
        public static ReceiveResult Signal(ReceiveOutcome outcome) => new(outcome, [], WebSocketCloseStatus.Empty, string.Empty);

        public static ReceiveResult Frame(byte[] payload) => new(ReceiveOutcome.Frame, payload, WebSocketCloseStatus.Empty, string.Empty);

        public static ReceiveResult ClientClose(WebSocketCloseStatus status, string reason) => new(ReceiveOutcome.ClientClose, [], status, reason);
    }
}
