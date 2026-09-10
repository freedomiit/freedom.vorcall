using System.Collections.Concurrent;
using System.Net.WebSockets;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

public enum MembershipCheck
{
    UnknownRoom,

    // The connection was replaced by a newer one of the same account.
    Stale,
    NotAMember,
    Member,
}

public enum JoinOutcome
{
    UnknownRoom,
    Stale,
    AlreadyMember,
    Joined,
}

public enum LeaveOutcome
{
    UnknownRoom,
    Stale,
    NotAMember,
    Left,
}

// The connection set and the presence model in one place. Every membership change and every
// room broadcast runs under _gate, which is what gives PROTOCOL.md its ordering guarantee: a
// connection cannot see a ChatMessage, MemberJoined or MemberLeft before its own Welcome and
// initial RoomState, because those are enqueued under the same lock.
public sealed class ConnectionRegistry(ILogger<ConnectionRegistry> logger)
{
    public const string GeneralRoomId = "general";

    private readonly Lock _gate = new();

    // Every accepted socket, whether or not it finished its handshake.
    private readonly ConcurrentDictionary<Guid, ClientConnection> _connections = new();

    // One live connection per account; both guarded by _gate.
    private readonly Dictionary<long, ClientConnection> _online = [];
    private readonly Dictionary<string, Room> _rooms = new() { [GeneralRoomId] = new Room(GeneralRoomId) };

    public int Count => _connections.Count;

    public void Add(ClientConnection connection) => _connections[connection.Id] = connection;

    public void Remove(ClientConnection connection) => _connections.TryRemove(connection.Id, out _);

    // Publishes the connection as the account's live one and queues its Welcome and RoomState
    // frames. Returns the connection it replaced, if any: the caller fails that one outside the
    // lock, because a dead socket must never hold up the account's new session.
    public ClientConnection? Attach(ClientConnection connection, long userId, string username, long latestMessageId)
    {
        List<ClientConnection>? slow = null;
        ClientConnection? replaced;
        lock (_gate)
        {
            // Before any membership write: RoomState reads Username straight off the members.
            connection.MarkReady(userId, username);

            _online.TryGetValue(userId, out replaced);
            if (replaced is not null)
            {
                // The account keeps the rooms it was in and the swap is silent, so nobody else
                // sees a leave/join pair for what is really one session moving.
                foreach (var room in _rooms.Values)
                {
                    if (Holds(room, replaced, userId))
                    {
                        room.Members[userId] = connection;
                    }
                }
            }

            // Every connection is a member of general from Hello on, so a session that had left
            // it joins again here, announced to the room like any other join.
            var general = _rooms[GeneralRoomId];
            var joinedGeneral = !Holds(general, connection, userId);
            if (joinedGeneral)
            {
                general.Members[userId] = connection;
            }

            _online[userId] = connection;

            Enqueue(
                connection,
                new ServerFrame { Welcome = new Welcome { LatestMessageId = latestMessageId, MemberId = userId, Username = username } },
                ref slow);

            // general first, then whatever else a replaced session handed over.
            EnqueueRoomStates(connection, userId, ref slow);

            if (joinedGeneral)
            {
                BroadcastLocked(
                    general,
                    new ServerFrame { MemberJoined = new MemberJoined { RoomId = GeneralRoomId, Member = MemberOf(connection, userId) } },
                    except: userId,
                    ref slow);
            }
        }

        CloseSlow(slow);
        return replaced;
    }

    // Announces the end of a session. A connection that was already replaced is not the
    // account's live one and leaves nothing behind: its rooms belong to its successor.
    public void Detach(ClientConnection connection)
    {
        if (connection.UserId is not { } userId)
        {
            return;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId))
            {
                return;
            }

            _online.Remove(userId);
            foreach (var room in _rooms.Values)
            {
                if (!Holds(room, connection, userId))
                {
                    continue;
                }

                room.Members.Remove(userId);
                BroadcastLocked(
                    room,
                    new ServerFrame { MemberLeft = new MemberLeft { RoomId = room.Id, UserId = userId } },
                    except: null,
                    ref slow);
            }
        }

        CloseSlow(slow);
    }

    public MembershipCheck Check(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return MembershipCheck.Stale;
        }

        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return MembershipCheck.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return MembershipCheck.Stale;
            }

            return Holds(room, connection, userId) ? MembershipCheck.Member : MembershipCheck.NotAMember;
        }
    }

    public JoinOutcome Join(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return JoinOutcome.Stale;
        }

        List<ClientConnection>? slow = null;
        JoinOutcome outcome;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return JoinOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return JoinOutcome.Stale;
            }

            if (Holds(room, connection, userId))
            {
                outcome = JoinOutcome.AlreadyMember;
            }
            else
            {
                room.Members[userId] = connection;
                outcome = JoinOutcome.Joined;
                BroadcastLocked(
                    room,
                    new ServerFrame { MemberJoined = new MemberJoined { RoomId = room.Id, Member = MemberOf(connection, userId) } },
                    except: userId,
                    ref slow);
            }

            // Both outcomes answer with the full membership: joining a room twice is the
            // client's resync primitive.
            Enqueue(connection, new ServerFrame { RoomState = StateOf(room) }, ref slow);
        }

        CloseSlow(slow);
        return outcome;
    }

    public LeaveOutcome Leave(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return LeaveOutcome.Stale;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return LeaveOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return LeaveOutcome.Stale;
            }

            if (!Holds(room, connection, userId))
            {
                return LeaveOutcome.NotAMember;
            }

            // Broadcast before the removal: PROTOCOL.md has the leaver receive its own MemberLeft.
            BroadcastLocked(
                room,
                new ServerFrame { MemberLeft = new MemberLeft { RoomId = room.Id, UserId = userId } },
                except: null,
                ref slow);
            room.Members.Remove(userId);
        }

        CloseSlow(slow);
        return LeaveOutcome.Left;
    }

    public void BroadcastToRoom(string roomId, ServerFrame frame)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_rooms.TryGetValue(roomId, out var room))
            {
                BroadcastLocked(room, frame, except: null, ref slow);
            }
        }

        CloseSlow(slow);
    }

    public Task CloseAllAsync(WebSocketCloseStatus status, string reason)
        => Task.WhenAll(_connections.Values.Select(connection => CloseQuietlyAsync(connection, status, reason)));

    private static void BroadcastLocked(Room room, ServerFrame frame, long? except, ref List<ClientConnection>? slow)
    {
        foreach (var (userId, member) in room.Members)
        {
            if (userId != except)
            {
                Enqueue(member, frame, ref slow);
            }
        }
    }

    // Never awaits a socket: a connection that cannot take the frame is collected and closed
    // after the lock is released, so one slow client cannot stall the room.
    private static void Enqueue(ClientConnection connection, ServerFrame frame, ref List<ClientConnection>? slow)
    {
        // CloseAsync clears IsReady before it stops accepting frames, so a refusal that comes
        // with IsReady still set is a full outbox and nothing else.
        if (connection.TryEnqueue(frame) || !connection.IsReady)
        {
            return;
        }

        (slow ??= []).Add(connection);
    }

    // general first, then the rest in insertion order, so the client's primary room is
    // populated before anything a replaced session handed over.
    private void EnqueueRoomStates(ClientConnection connection, long userId, ref List<ClientConnection>? slow)
    {
        var general = _rooms[GeneralRoomId];
        if (Holds(general, connection, userId))
        {
            Enqueue(connection, new ServerFrame { RoomState = StateOf(general) }, ref slow);
        }

        foreach (var room in _rooms.Values)
        {
            if (room.Id != GeneralRoomId && Holds(room, connection, userId))
            {
                Enqueue(connection, new ServerFrame { RoomState = StateOf(room) }, ref slow);
            }
        }
    }

    private static bool Holds(Room room, ClientConnection connection, long userId)
        => room.Members.TryGetValue(userId, out var member) && ReferenceEquals(member, connection);

    private static RoomState StateOf(Room room)
    {
        var state = new RoomState { RoomId = room.Id };
        foreach (var (userId, member) in room.Members)
        {
            state.Members.Add(MemberOf(member, userId));
        }

        return state;
    }

    private static Member MemberOf(ClientConnection connection, long userId)
        => new() { UserId = userId, Username = connection.Username ?? string.Empty };

    private bool IsLive(ClientConnection connection, long userId)
        => _online.TryGetValue(userId, out var live) && ReferenceEquals(live, connection);

    private void CloseSlow(List<ClientConnection>? slow)
    {
        if (slow is null)
        {
            return;
        }

        foreach (var connection in slow)
        {
            logger.LogWarning(
                "Connection {ConnectionId} (user {UserId} {Username}) fell behind {Capacity} queued frames; closing",
                connection.Id,
                connection.UserId,
                connection.Username,
                ClientConnection.OutboxCapacity);
            _ = CloseQuietlyAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
        }
    }

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

    private sealed class Room(string id)
    {
        public string Id { get; } = id;

        public Dictionary<long, ClientConnection> Members { get; } = [];
    }
}
