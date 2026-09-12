using System.Net.WebSockets;
using System.Text;
using System.Threading.Channels;
using Google.Protobuf;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

// RFC 6455 leaves 1013 to the application, so WebSocketCloseStatus has no member for it, and
// 4000..4999 is the private range PROTOCOL.md spends on the two admin closes.
internal static class VorcallCloseStatus
{
    public const WebSocketCloseStatus SlowConsumer = (WebSocketCloseStatus)1013;

    public const WebSocketCloseStatus Kicked = (WebSocketCloseStatus)4001;

    // The owner's account lock (`users disable`), not the in-app moderation ban.
    public const WebSocketCloseStatus Disabled = (WebSocketCloseStatus)4003;
}

// One of these per accepted socket. Everything the server sends goes through the outbox so
// that exactly one writer touches the socket and frame order is preserved.
public sealed class ClientConnection : IDisposable
{
    public const int OutboxCapacity = 256;

    private const int MaxCloseReasonBytes = 123;
    private static readonly TimeSpan CloseTimeout = TimeSpan.FromSeconds(5);

    private readonly System.Threading.Channels.Channel<ServerFrame> _outbox =
        System.Threading.Channels.Channel.CreateBounded<ServerFrame>(new BoundedChannelOptions(OutboxCapacity) { SingleReader = true });

    private readonly ILogger _logger;
    private readonly CancellationTokenSource _lifetime;
    private readonly CancellationTokenSource _closing = new();
    private readonly Task _sendLoop;
    private readonly Lock _closeGate = new();
    private Task? _closeTask;
    private volatile bool _isReady;

    public ClientConnection(WebSocket socket, ILogger logger, CancellationToken requestAborted, CancellationToken applicationStopping)
    {
        Socket = socket;
        _logger = logger;
        _lifetime = CancellationTokenSource.CreateLinkedTokenSource(requestAborted, applicationStopping);
        _sendLoop = Task.Run(SendLoopAsync);
    }

    public Guid Id { get; } = Guid.NewGuid();

    public WebSocket Socket { get; }

    // Both set by MarkReady, before IsReady turns on: an identity is never half-published.
    public long? UserId { get; private set; }

    public string? Username { get; private set; }

    public bool IsReady => _isReady;

    // Cancelled when the request is aborted or the host starts shutting down; the receive
    // loop watches it. The send loop deliberately does not (see SendLoopAsync).
    public CancellationToken Lifetime => _lifetime.Token;

    // Cancelled as soon as a close is latched, so the receive loop stops acting on a connection
    // that is already going away instead of waiting out its idle deadline in CloseSent.
    public CancellationToken Closing => _closing.Token;

    // The close code and reason that were actually latched, which is not necessarily what a
    // later caller asked for: the first caller wins and the rest await that same close.
    public WebSocketCloseStatus? CloseStatus { get; private set; }

    public string? CloseReason { get; private set; }

    public void MarkReady(long userId, string username)
    {
        UserId = userId;
        Username = username;
        _isReady = true;
    }

    // False means the outbox is full: the caller closes the connection as a slow consumer.
    public bool TryEnqueue(ServerFrame frame) => _outbox.Writer.TryWrite(frame);

    public async Task FailAsync(Error error, WebSocketCloseStatus status, string reason)
    {
        // The error frame must reach the peer before the close frame. CloseAsync drains the
        // outbox before it closes, so enqueueing first is enough to guarantee the order.
        _outbox.Writer.TryWrite(new ServerFrame { Error = error });
        await CloseAsync(status, reason);
    }

    // Idempotent: the first caller decides the close code, everyone else awaits that same
    // close instead of racing it or returning while the socket is still being shut down.
    public Task CloseAsync(WebSocketCloseStatus status, string reason)
    {
        lock (_closeGate)
        {
            if (_closeTask is null)
            {
                CloseStatus = status;
                CloseReason = reason;
                _closeTask = CloseCoreAsync(status, reason);
            }

            return _closeTask;
        }
    }

    public void Dispose()
    {
        _lifetime.Dispose();
        _closing.Dispose();
    }

    private async Task CloseCoreAsync(WebSocketCloseStatus status, string reason)
    {
        _isReady = false;
        _outbox.Writer.TryComplete();

        // Ahead of the outbox wait: the receive loop has to stop here, not once the peer answers
        // the close frame or the idle deadline fires. CancelAsync and not Cancel, because Cancel
        // resumes that loop inline, which can re-enter CloseAsync before _closeTask is latched.
        await _closing.CancelAsync();

        if (!await WaitForOutboxAsync())
        {
            // A send that never completes would also block the close handshake. Aborting
            // unblocks the send loop and tears the socket down.
            _logger.LogDebug("Connection {ConnectionId}: outbox did not drain, aborting socket", Id);
            Socket.Abort();
            return;
        }

        try
        {
            if (Socket.State is WebSocketState.Open or WebSocketState.CloseReceived)
            {
                using var timeout = new CancellationTokenSource(CloseTimeout);
                // CloseOutputAsync, not CloseAsync: waiting here for the peer's answering close
                // frame would collide with the receive loop, which is the reader that picks it up.
                await Socket.CloseOutputAsync(status, TruncateReason(reason), timeout.Token);
            }
        }
        catch (Exception ex) when (ex is WebSocketException or OperationCanceledException or ObjectDisposedException or InvalidOperationException)
        {
            _logger.LogDebug(ex, "Connection {ConnectionId}: close handshake failed, aborting socket", Id);
            Socket.Abort();
        }
    }

    // Not bound to the lifetime token on purpose: at shutdown the outbox still has to drain
    // so the peer gets its queued frames and a real close frame instead of an abort.
    private async Task SendLoopAsync()
    {
        try
        {
            while (await _outbox.Reader.WaitToReadAsync())
            {
                while (_outbox.Reader.TryRead(out var frame))
                {
                    var payload = frame.ToByteArray();
                    await Socket.SendAsync(new ArraySegment<byte>(payload), WebSocketMessageType.Binary, endOfMessage: true, CancellationToken.None);
                }
            }
        }
        catch (Exception ex) when (ex is WebSocketException or OperationCanceledException or ObjectDisposedException)
        {
            _logger.LogDebug(ex, "Connection {ConnectionId}: outbound stream ended", Id);
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Connection {ConnectionId}: unexpected send failure", Id);
        }
    }

    private async Task<bool> WaitForOutboxAsync()
    {
        var finished = await Task.WhenAny(_sendLoop, Task.Delay(CloseTimeout));
        if (finished != _sendLoop)
        {
            return false;
        }

        // The send loop logs and swallows its own failures, so this only observes completion.
        await _sendLoop;
        return true;
    }

    // A close reason travels in a control frame: 123 bytes at most. Reasons mirrored back
    // from a client can be longer than that.
    private static string TruncateReason(string reason)
    {
        if (Encoding.UTF8.GetByteCount(reason) <= MaxCloseReasonBytes)
        {
            return reason;
        }

        var bytes = Encoding.UTF8.GetBytes(reason);
        var length = MaxCloseReasonBytes;
        while (length > 0 && (bytes[length] & 0xC0) == 0x80)
        {
            length--;
        }

        return Encoding.UTF8.GetString(bytes, 0, length);
    }
}
