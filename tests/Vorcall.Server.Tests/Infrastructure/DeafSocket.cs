using System.Diagnostics;
using System.Net.WebSockets;
using Google.Protobuf;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Protocol;
using Xunit;

namespace Vorcall.Server.Tests.Infrastructure;

// A socket that says hello and then reads nothing. WsClient's pump keeps the server's outbox
// draining, which is exactly what makes a full one unreachable from there; this is the client a
// test needs when the point is what the server does with a member that has stopped listening.
internal sealed class DeafSocket : IAsyncDisposable
{
    private readonly WebSocket _socket;

    private DeafSocket(WebSocket socket, Account account, ClientConnection connection)
    {
        _socket = socket;
        Account = account;
        Connection = connection;
    }

    public Account Account { get; }

    // The server's own side of this socket, which is where the outbox is.
    public ClientConnection Connection { get; }

    public static async Task<DeafSocket> ConnectAsync(VorcallFactory factory, Account account)
    {
        var client = factory.Server.CreateWebSocketClient();
        client.ConfigureRequest = request =>
        {
            request.Headers[ServerKeyMiddleware.HeaderName] = ServerFixture.ServerKey;
            request.Headers.Authorization = $"Bearer {account.Access}";
        };

        var socket = await client.ConnectAsync(new Uri(factory.Server.BaseAddress, "/ws"), CancellationToken.None);
        await socket.SendAsync(
            new ArraySegment<byte>(Frames.Hello().ToByteArray()),
            WebSocketMessageType.Binary,
            endOfMessage: true,
            CancellationToken.None);

        // A Pong is the usual fence for "the hello has been acted on", and this socket will never
        // read one, so the registry's own view of the account is the fence instead.
        var registry = factory.Services.GetRequiredService<ConnectionRegistry>();
        var deadline = Stopwatch.StartNew();
        while (registry.LiveConnection(account.UserId) is not { } connection)
        {
            Assert.True(deadline.Elapsed < WsClient.DefaultTimeout, $"{account.Username}: never came online");
            await Task.Delay(20);
        }

        return new DeafSocket(socket, account, registry.LiveConnection(account.UserId)!);
    }

    // Holds the outbox at capacity until the returned handle is disposed. A client that reads
    // nothing does not by itself fill the outbox — the send loop keeps draining into the test
    // transport, which buffers without limit — so a full one has to be held there by writers that
    // outrun that loop: a TryEnqueue is a queue write, while taking one costs a serialisation and
    // a hand-off. They win most of the time and not all of it, which is why the caller retries.
    public Pressure HoldOutboxFull() => new(Connection);

    internal sealed class Pressure : IDisposable
    {
        private const int Writers = 3;

        // Large enough that serialising one costs the send loop far more than enqueuing one costs
        // a writer, and small enough that the frames left in the transport are megabytes and not
        // gigabytes.
        private static readonly ServerFrame Filler =
            new() { Error = new Error { Code = ErrorCode.Unspecified, Detail = new string('f', 8192) } };

        private readonly CancellationTokenSource _stop = new();
        private readonly Task[] _writers;

        internal Pressure(ClientConnection connection)
        {
            _writers = [.. Enumerable.Range(0, Writers).Select(_ => Task.Factory.StartNew(
                () =>
                {
                    while (!_stop.IsCancellationRequested)
                    {
                        connection.TryEnqueue(Filler);
                    }
                },
                TaskCreationOptions.LongRunning))];
        }

        public void Dispose()
        {
            _stop.Cancel();
            Task.WaitAll(_writers);
            _stop.Dispose();
        }
    }

    // Reads everything queued and hands back the code the server closed with. Only useful after
    // the outbox has been filled: the close cannot finish until the send loop has drained.
    public async Task<int> DrainToCloseAsync(TimeSpan? timeout = null)
    {
        using var cts = new CancellationTokenSource(timeout ?? TimeSpan.FromSeconds(20));
        var buffer = new byte[64 * 1024];
        while (true)
        {
            WebSocketReceiveResult result;
            try
            {
                result = await _socket.ReceiveAsync(new ArraySegment<byte>(buffer), cts.Token);
            }
            catch (OperationCanceledException)
            {
                throw new TimeoutException($"{Account.Username}: the socket never closed");
            }

            if (result.MessageType == WebSocketMessageType.Close)
            {
                return (int?)result.CloseStatus ?? 1005;
            }
        }
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (_socket.State == WebSocketState.Open)
            {
                await _socket.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "done", CancellationToken.None);
            }
        }
        catch (Exception ex) when (ex is WebSocketException or ObjectDisposedException or InvalidOperationException)
        {
            // Already torn down by the server.
        }

        _socket.Dispose();
    }
}
