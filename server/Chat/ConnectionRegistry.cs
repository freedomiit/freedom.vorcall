using System.Collections.Concurrent;
using System.Net.WebSockets;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

public sealed class ConnectionRegistry(ILogger<ConnectionRegistry> logger)
{
    private readonly ConcurrentDictionary<Guid, ClientConnection> _connections = new();

    public int Count => _connections.Count;

    public void Add(ClientConnection connection) => _connections[connection.Id] = connection;

    public void Remove(ClientConnection connection) => _connections.TryRemove(connection.Id, out _);

    // Never awaits a socket: a connection that cannot take the frame is closed on a detached
    // task so that one slow client cannot stall the room.
    public void Broadcast(ServerFrame frame)
    {
        foreach (var connection in _connections.Values)
        {
            if (!connection.IsReady || connection.TryEnqueue(frame))
            {
                continue;
            }

            // CloseAsync clears IsReady before it stops accepting frames, so a refusal that
            // comes with IsReady still set is a full outbox and nothing else.
            if (!connection.IsReady)
            {
                continue;
            }

            logger.LogWarning(
                "Connection {ConnectionId} ({Nickname}) fell behind {Capacity} queued frames; closing",
                connection.Id,
                connection.Nickname,
                ClientConnection.OutboxCapacity);
            _ = CloseQuietlyAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
        }
    }

    public Task CloseAllAsync(WebSocketCloseStatus status, string reason)
        => Task.WhenAll(_connections.Values.Select(connection => CloseQuietlyAsync(connection, status, reason)));

    private async Task CloseQuietlyAsync(ClientConnection connection, WebSocketCloseStatus status, string reason)
    {
        try
        {
            await connection.CloseAsync(status, reason);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Connection {ConnectionId}: failed to close with {CloseCode}", connection.Id, (int)status);
        }
    }
}
