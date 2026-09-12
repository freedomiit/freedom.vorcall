using System.Collections.Concurrent;
using System.Net.WebSockets;
using Google.Protobuf;
using Vorcall.Server.Protocol;
using Vorcall.Server.Voice;

namespace Vorcall.Server.Chat;

public enum MembershipCheck
{
    UnknownRoom,

    // The connection was replaced by a newer one of the same account.
    Stale,
    NotAMember,
    Member,
}

// What an account may do with a room, asked before anything is written: a room the caller
// cannot see must not cause a database round trip either.
public enum RoomAccess
{
    UnknownRoom,
    Stale,

    // A DM the caller is not part of, whose existence is not admitted to anyone else.
    Forbidden,
    NotAMember,
    Member,
}

public enum JoinOutcome
{
    UnknownRoom,
    Stale,
    Forbidden,

    // Already a persistent member: joining again is the client's resync primitive.
    Resynced,
    Joined,
}

public enum LeaveOutcome
{
    UnknownRoom,
    Stale,

    // general and DMs are permanent.
    Forbidden,
    NotAMember,
    Left,
}

public enum CreateRoomOutcome
{
    Stale,
    Created,
}

public enum JoinVoiceOutcome
{
    UnknownRoom,
    Stale,
    NotAMember,
    Unavailable,
    Rejoined,
    Joined,
}

public enum LeaveVoiceOutcome
{
    UnknownRoom,
    Stale,
    NotInVoice,
    Left,
}

public enum StartShareOutcome
{
    UnknownRoom,
    Stale,
    NotInVoice,

    // Sharing is switched off on this server, or the relay never came up.
    Unavailable,

    // The room already holds as many sharers as the relay allows.
    Limit,
    Started,
}

public enum StopShareOutcome
{
    UnknownRoom,
    Stale,
    NotInVoice,
    NotSharing,
    Stopped,
}

public enum WatchShareOutcome
{
    UnknownRoom,
    Stale,
    NotInVoice,

    // The target is not in the channel, is not sharing, or is the caller itself.
    NotSharing,
    Watching,
}

public enum UnwatchShareOutcome
{
    UnknownRoom,
    Stale,
    NotInVoice,
    Unwatched,
}

// Everything a scrape wants to know about the presence model, read in one pass so the six
// figures describe the same moment rather than six different ones.
public readonly record struct RegistrySnapshot(
    int Connections,
    int OnlineUsers,
    int Rooms,
    int VoiceSessions,
    int Sharers,
    int Watchers);

// The connection set and the presence model in one place. Every membership change and every
// room broadcast runs under _gate, which is what gives PROTOCOL.md its ordering guarantee: a
// connection cannot see a ChatMessage, MemberJoined or MemberLeft before its own Welcome and
// initial RoomState, because those are enqueued under the same lock. Voice membership lives
// under the same lock, so a voice frame can never overtake the RoomState of its own room.
//
// Rooms and their memberships are persistent rows; this class holds a mirror of them and never
// touches the database itself. Every caller writes the row first and then tells the mirror.
public sealed class ConnectionRegistry
{
    public const string GeneralRoomId = "general";

    private readonly Lock _gate = new();

    // Every accepted socket, whether or not it finished its handshake.
    private readonly ConcurrentDictionary<Guid, ClientConnection> _connections = new();

    // One live connection per account; all three guarded by _gate. _order holds the same rooms
    // as _rooms with general first, which is the order PROTOCOL.md sends their RoomState in.
    private readonly Dictionary<long, ClientConnection> _online = [];
    private readonly Dictionary<string, Room> _rooms = [];
    private readonly List<Room> _order = [];

    private readonly RoomDirectory _directory;
    private readonly VoiceRelay _relay;
    private readonly ILogger<ConnectionRegistry> _logger;

    public ConnectionRegistry(RoomDirectory directory, VoiceRelay relay, ILogger<ConnectionRegistry> logger)
    {
        _directory = directory;
        _relay = relay;
        _logger = logger;
        _relay.SpeakingChanged += OnSpeakingChanged;
    }

    public int Count => _connections.Count;

    // One pass under the lock every membership change already takes: a scrape costs the rooms a
    // single traversal and never holds the gate across anything that can block.
    public RegistrySnapshot Snapshot()
    {
        lock (_gate)
        {
            var voiceSessions = 0;
            var sharers = 0;
            var watchers = 0;

            foreach (var room in _rooms.Values)
            {
                voiceSessions += room.Voice.Count;
                foreach (var slot in room.Voice.Values)
                {
                    if (slot.Sharing)
                    {
                        sharers++;
                    }

                    if (slot.Watching is not null)
                    {
                        watchers++;
                    }
                }
            }

            return new RegistrySnapshot(_connections.Count, _online.Count, _rooms.Count, voiceSessions, sharers, watchers);
        }
    }

    public void Add(ClientConnection connection) => _connections[connection.Id] = connection;

    public void Remove(ClientConnection connection) => _connections.TryRemove(connection.Id, out _);

    // Called once at startup, before anything listens: a room that is not in the mirror cannot
    // be joined, sent to or read, so the mirror has to be complete before the first Hello.
    public async Task LoadRoomsAsync()
    {
        var rooms = await _directory.LoadAllAsync();
        int count;
        lock (_gate)
        {
            _rooms.Clear();
            _order.Clear();

            // general sorts nowhere in particular by id, and its RoomState goes out first.
            var general = rooms.FirstOrDefault(entry => entry.Room.Id == GeneralRoomId);
            if (general.Room is null)
            {
                throw new InvalidOperationException($"Room '{GeneralRoomId}' is missing: the database is not migrated.");
            }

            MirrorLocked(general.Room, general.MemberIds);
            foreach (var (record, memberIds) in rooms)
            {
                if (record.Id != GeneralRoomId)
                {
                    MirrorLocked(record, memberIds);
                }
            }

            count = _order.Count;
        }

        _logger.LogInformation("Mirrored {RoomCount} rooms", count);
    }

    // Publishes the connection as the account's live one and queues its Welcome, RoomState and
    // RoomList frames. The entries are the account's own rooms as the database has them, read by
    // the caller a moment ago. Returns the connection it replaced, if any: the caller fails that
    // one outside the lock, because a dead socket must never hold up the account's new session.
    public ClientConnection? Attach(
        ClientConnection connection,
        long userId,
        string username,
        long latestMessageId,
        IReadOnlyList<RoomEntryRecord> entries)
    {
        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<Room> joined = [];
        ClientConnection? replaced;
        lock (_gate)
        {
            // Before any membership write: RoomState reads Username straight off the members.
            connection.MarkReady(userId, username);

            // The entries are what teaches the mirror about a membership written outside the
            // registry, a fresh account's general membership above all. Reconciled both ways
            // against that read, because a leave whose Leave call came back Stale deleted the row
            // and then found the session already replaced: nothing else would ever drop it from
            // the mirror, and a Hello is the next moment the database truth is in hand.
            foreach (var entry in entries)
            {
                if (!_rooms.TryGetValue(entry.Room.Id, out var mirrored))
                {
                    mirrored = MirrorLocked(entry.Room, entry.MemberIds);
                }

                if (entry.MemberIds.Contains(userId))
                {
                    mirrored.MemberIds.Add(userId);
                }
                else
                {
                    // Silent: a room the account is not in has no audience owed a MemberLeft, and
                    // the leave that dropped the row already had its chance to announce itself.
                    mirrored.MemberIds.Remove(userId);
                    mirrored.Members.Remove(userId);
                }
            }

            _online.TryGetValue(userId, out replaced);
            if (replaced is not null)
            {
                // The account keeps the rooms it was in and the swap is silent, so nobody else
                // sees a leave/join pair for what is really one session moving.
                foreach (var room in _order)
                {
                    // Voice first, and never transferred: the key and ssrc belong to the socket
                    // that proved its address. Announcing it before the text slot changes hands
                    // also keeps the leave off the new connection's outbox, which owes Welcome
                    // its first frame.
                    if (room.Voice.TryGetValue(userId, out var slot) && ReferenceEquals(slot.Connection, replaced))
                    {
                        RemoveVoiceLocked(room, userId, ref slow, removed);
                    }

                    if (Holds(room, replaced, userId))
                    {
                        room.Members[userId] = connection;
                    }
                }
            }

            // Membership is persistent and presence is per connection; this is where the two
            // meet, so every room the account belongs to gets this connection as its live slot.
            foreach (var room in _order)
            {
                if (room.MemberIds.Contains(userId) && !Holds(room, connection, userId))
                {
                    room.Members[userId] = connection;
                    joined.Add(room);
                }
            }

            _online[userId] = connection;

            Enqueue(
                connection,
                new ServerFrame { Welcome = new Welcome { LatestMessageId = latestMessageId, MemberId = userId, Username = username } },
                ref slow);

            // Every room state before the list that names those rooms, and both before anyone
            // else hears about the arrival.
            EnqueueRoomStates(connection, userId, ref slow);
            Enqueue(connection, RoomListOf(entries), ref slow);

            // A replacement is silent: the account never left, so nobody is told it arrived.
            if (replaced is null)
            {
                foreach (var room in joined)
                {
                    BroadcastLocked(
                        room,
                        new ServerFrame { MemberJoined = new MemberJoined { RoomId = room.Id, Member = MemberOf(connection, userId) } },
                        except: userId,
                        ref slow);
                }
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return replaced;
    }

    // Announces the end of a session. A connection that was already replaced is not the
    // account's live one and leaves nothing behind: its rooms belong to its successor. The
    // memberships themselves are persistent and survive the disconnection.
    public void Detach(ClientConnection connection)
    {
        if (connection.UserId is not { } userId)
        {
            return;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        lock (_gate)
        {
            if (!IsLive(connection, userId))
            {
                return;
            }

            _online.Remove(userId);
            foreach (var room in _order)
            {
                if (!Holds(room, connection, userId))
                {
                    continue;
                }

                room.Members.Remove(userId);

                // VoiceMemberLeft before MemberLeft: the room stops hearing the session before
                // it stops seeing the member.
                RemoveVoiceLocked(room, userId, ref slow, removed);
                BroadcastLocked(
                    room,
                    new ServerFrame { MemberLeft = new MemberLeft { RoomId = room.Id, UserId = userId } },
                    except: null,
                    ref slow);
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
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

    // The persistent membership, which an account keeps while it is offline: the history
    // endpoint and MarkRead ask about the account, not about a live connection.
    public bool IsMember(string roomId, long userId)
    {
        lock (_gate)
        {
            return _rooms.TryGetValue(roomId, out var room) && room.MemberIds.Contains(userId);
        }
    }

    public RoomAccess Access(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return RoomAccess.Stale;
        }

        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return RoomAccess.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return RoomAccess.Stale;
            }

            if (room.MemberIds.Contains(userId))
            {
                return RoomAccess.Member;
            }

            return room.Kind == Data.RoomKind.Dm ? RoomAccess.Forbidden : RoomAccess.NotAMember;
        }
    }

    // The membership row is written before this call, so finding the user already in the mirror
    // is the normal path of a resync and stays a no-op.
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

            if (room.Kind == Data.RoomKind.Dm && !room.MemberIds.Contains(userId))
            {
                return JoinOutcome.Forbidden;
            }

            var joined = room.MemberIds.Add(userId);
            room.Members[userId] = connection;
            if (joined)
            {
                BroadcastLocked(
                    room,
                    new ServerFrame { MemberJoined = new MemberJoined { RoomId = room.Id, Member = MemberOf(connection, userId) } },
                    except: userId,
                    ref slow);
            }

            // Both outcomes answer with the full membership: joining a room twice is the
            // client's resync primitive.
            EnqueueState(connection, room, ref slow);

            // After the joiner's own RoomState, so the room it names is already described.
            if (joined)
            {
                BroadcastToAllLocked(RoomUpdatedOf(room), ref slow);
            }

            outcome = joined ? JoinOutcome.Joined : JoinOutcome.Resynced;
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
        List<uint> removed = [];
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

            if (room.Id == GeneralRoomId || room.Kind == Data.RoomKind.Dm)
            {
                return LeaveOutcome.Forbidden;
            }

            if (!room.MemberIds.Contains(userId))
            {
                return LeaveOutcome.NotAMember;
            }

            // Leaving the text room leaves its voice channel too, announced first.
            RemoveVoiceLocked(room, userId, ref slow, removed);

            // Broadcast before the removal: PROTOCOL.md has the leaver receive its own MemberLeft.
            BroadcastLocked(
                room,
                new ServerFrame { MemberLeft = new MemberLeft { RoomId = room.Id, UserId = userId } },
                except: null,
                ref slow);
            room.Members.Remove(userId);
            room.MemberIds.Remove(userId);
            BroadcastToAllLocked(RoomUpdatedOf(room), ref slow);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return LeaveOutcome.Left;
    }

    // The room and the creator's membership are persisted before this call.
    public CreateRoomOutcome CreateRoom(ClientConnection connection, RoomRecord record)
    {
        if (connection.UserId is not { } userId)
        {
            return CreateRoomOutcome.Stale;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId))
            {
                return CreateRoomOutcome.Stale;
            }

            if (!_rooms.TryGetValue(record.Id, out var room))
            {
                room = MirrorLocked(record, [userId]);
            }

            room.MemberIds.Add(userId);
            room.Members[userId] = connection;
            EnqueueState(connection, room, ref slow);
            BroadcastToAllLocked(RoomUpdatedOf(room), ref slow);
        }

        CloseSlow(slow);
        return CreateRoomOutcome.Created;
    }

    // The room and both memberships are persisted before this call. A DM concerns nobody but its
    // two members, so nothing about it is broadcast further than they are.
    public void OpenDm(ClientConnection caller, RoomRecord record, long otherId, bool created)
    {
        if (caller.UserId is not { } callerId)
        {
            return;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            // A replaced connection gets nothing and, above all, takes no slot: the account's
            // live session owns those, and it mirrors the DM from its own entries at Hello.
            if (!IsLive(caller, callerId))
            {
                return;
            }

            if (!_rooms.TryGetValue(record.Id, out var room))
            {
                room = MirrorLocked(record, [callerId, otherId]);
            }
            else
            {
                room.MemberIds.Add(callerId);
                room.MemberIds.Add(otherId);
            }

            if (created)
            {
                foreach (var memberId in room.MemberIds)
                {
                    if (_online.TryGetValue(memberId, out var member))
                    {
                        room.Members[memberId] = member;
                        EnqueueState(member, room, ref slow);
                    }
                }

                BroadcastToUsersLocked(room.MemberIds, RoomUpdatedOf(room), ref slow);
            }
            else
            {
                // The DM was already there, so only the caller asked for anything.
                room.Members[callerId] = caller;
                EnqueueState(caller, room, ref slow);
            }
        }

        CloseSlow(slow);
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

    public JoinVoiceOutcome JoinVoice(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return JoinVoiceOutcome.Stale;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        JoinVoiceOutcome outcome;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return JoinVoiceOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return JoinVoiceOutcome.Stale;
            }

            // Voice rides on the text room: its audience is the text membership, so there is
            // nobody to announce a voice join to before the text join happened.
            if (!Holds(room, connection, userId))
            {
                return JoinVoiceOutcome.NotAMember;
            }

            if (!_relay.IsAvailable)
            {
                return JoinVoiceOutcome.Unavailable;
            }

            // Repeating JoinVoice is safe, but it is never a no-op: the caller's old session
            // ends and a fresh one replaces it. A client that asks again has rebuilt its media
            // engine with the sequence counter back at zero, so handing back the same key would
            // repeat (key, nonce) pairs that the relay's replay window already refuses. Every
            // VoiceReady therefore carries a key and ssrc that were never used before.
            outcome = room.Voice.ContainsKey(userId) ? JoinVoiceOutcome.Rejoined : JoinVoiceOutcome.Joined;
            RemoveVoiceLocked(room, userId, ref slow, removed);

            // CreateSession takes the relay's own lock and calls nothing back into here.
            var slot = new VoiceSlot(connection, _relay.CreateSession(room.Id, userId));
            room.Voice[userId] = slot;

            Enqueue(connection, VoiceReadyOf(room, slot.Session), ref slow);
            Enqueue(connection, VoiceStateOf(room), ref slow);
            BroadcastLocked(
                room,
                new ServerFrame { VoiceMemberJoined = new VoiceMemberJoined { RoomId = room.Id, Member = VoiceMemberOf(slot) } },
                except: userId,
                ref slow);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return outcome;
    }

    public LeaveVoiceOutcome LeaveVoice(ClientConnection connection, string roomId)
    {
        if (connection.UserId is not { } userId)
        {
            return LeaveVoiceOutcome.Stale;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return LeaveVoiceOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return LeaveVoiceOutcome.Stale;
            }

            if (!room.Voice.ContainsKey(userId))
            {
                return LeaveVoiceOutcome.NotInVoice;
            }

            RemoveVoiceLocked(room, userId, ref slow, removed);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return LeaveVoiceOutcome.Left;
    }

    // A share rides an existing voice session: the media travels on the ssrc the caller already
    // proved, so there is nothing to hand out here and the audience is the text room, which is
    // what already knows who is in the channel. Repeating StartShare is how a client re-announces
    // a share it is already running, which is why it neither counts against the room's ceiling
    // nor restarts anything on the relay.
    public StartShareOutcome StartShare(ClientConnection connection, long userId, string roomId, bool audio)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return StartShareOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return StartShareOutcome.Stale;
            }

            if (!room.Voice.TryGetValue(userId, out var slot))
            {
                return StartShareOutcome.NotInVoice;
            }

            if (!_relay.ShareEnabled)
            {
                return StartShareOutcome.Unavailable;
            }

            if (!slot.Sharing && room.Voice.Values.Count(other => other.Sharing) >= _relay.MaxSharersPerRoom)
            {
                return StartShareOutcome.Limit;
            }

            slot.Sharing = true;
            slot.ShareAudio = audio;

            // SetSharing takes the relay's own lock and calls nothing back into here.
            _relay.SetSharing(slot.Session.Ssrc, true, audio);

            BroadcastLocked(
                room,
                new ServerFrame { ShareStarted = new ShareStarted { RoomId = room.Id, UserId = userId, Audio = audio } },
                except: null,
                ref slow);

            // Watchers of a share that was already running keep their watch, so the count answered
            // here is the real one rather than zero.
            Enqueue(connection, ShareWatchersOf(room, userId), ref slow);
        }

        CloseSlow(slow);
        return StartShareOutcome.Started;
    }

    public StopShareOutcome StopShare(ClientConnection connection, long userId, string roomId)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return StopShareOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return StopShareOutcome.Stale;
            }

            if (!room.Voice.TryGetValue(userId, out var slot))
            {
                return StopShareOutcome.NotInVoice;
            }

            if (!slot.Sharing)
            {
                return StopShareOutcome.NotSharing;
            }

            StopShareLocked(room, userId, slot, ref slow);
        }

        CloseSlow(slow);
        return StopShareOutcome.Stopped;
    }

    // A viewer watches at most one share at a time, so this is a replacement: the share it leaves
    // loses a watcher and hears about it, and the share it joins gains one.
    public WatchShareOutcome WatchShare(ClientConnection connection, long userId, string roomId, long targetUserId)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return WatchShareOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return WatchShareOutcome.Stale;
            }

            if (!room.Voice.TryGetValue(userId, out var viewer))
            {
                return WatchShareOutcome.NotInVoice;
            }

            // Watching yourself is refused the same way as watching a stranger: the relay would
            // only clear the watch, and the client must be told nothing is coming.
            if (targetUserId == userId
                || !room.Voice.TryGetValue(targetUserId, out var target)
                || !target.Sharing)
            {
                return WatchShareOutcome.NotSharing;
            }

            if (viewer.Watching == targetUserId)
            {
                // Idempotent: nothing changed, so only the viewer's own answer is repeated and the
                // sharer is not told its count again.
                Enqueue(connection, WatchStateOf(room, targetUserId), ref slow);
            }
            else
            {
                if (viewer.Watching is { } previous)
                {
                    // Cleared before the count is read, so the previous sharer hears the figure
                    // that is true after this viewer left it.
                    viewer.Watching = null;
                    if (room.Voice.TryGetValue(previous, out var previousSharer))
                    {
                        Enqueue(previousSharer.Connection, ShareWatchersOf(room, previous), ref slow);
                    }
                }

                viewer.Watching = targetUserId;
                _relay.Watch(viewer.Session.Ssrc, target.Session.Ssrc);
                Enqueue(connection, WatchStateOf(room, targetUserId), ref slow);
                Enqueue(target.Connection, ShareWatchersOf(room, targetUserId), ref slow);
            }
        }

        CloseSlow(slow);
        return WatchShareOutcome.Watching;
    }

    public UnwatchShareOutcome UnwatchShare(ClientConnection connection, long userId, string roomId)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(roomId, out var room))
            {
                return UnwatchShareOutcome.UnknownRoom;
            }

            if (!IsLive(connection, userId))
            {
                return UnwatchShareOutcome.Stale;
            }

            if (!room.Voice.TryGetValue(userId, out var viewer))
            {
                return UnwatchShareOutcome.NotInVoice;
            }

            if (viewer.Watching is { } previous)
            {
                viewer.Watching = null;
                _relay.Watch(viewer.Session.Ssrc, null);
                if (room.Voice.TryGetValue(previous, out var sharer))
                {
                    Enqueue(sharer.Connection, ShareWatchersOf(room, previous), ref slow);
                }
            }

            // Answered even when nothing was being watched, so a client that believed otherwise is
            // corrected either way.
            Enqueue(connection, WatchStateOf(room, 0), ref slow);
        }

        CloseSlow(slow);
        return UnwatchShareOutcome.Unwatched;
    }

    // The admin endpoint's reach into a live session. Nothing is sent ahead of the close: the
    // client has to see the close code itself, where an Error frame would read as a failure to
    // reconnect from. Presence, voice and any share are released by the handler's own Detach
    // when its receive loop unwinds, exactly as on any other close.
    public async Task<bool> DisconnectAsync(long userId, WebSocketCloseStatus status, string reason)
    {
        ClientConnection? connection;
        lock (_gate)
        {
            _online.TryGetValue(userId, out connection);
        }

        if (connection is null)
        {
            return false;
        }

        _logger.LogInformation(
            "Admin closed the connection of user {UserId} with {CloseCode} {CloseReason}",
            userId,
            (int)status,
            reason);
        await CloseQuietlyAsync(connection, status, reason);
        return true;
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

    // A room list changed, which is not a room event: its audience is every live session rather
    // than one room's membership.
    private void BroadcastToAllLocked(ServerFrame frame, ref List<ClientConnection>? slow)
    {
        foreach (var connection in _online.Values)
        {
            Enqueue(connection, frame, ref slow);
        }
    }

    private void BroadcastToUsersLocked(IEnumerable<long> userIds, ServerFrame frame, ref List<ClientConnection>? slow)
    {
        foreach (var userId in userIds)
        {
            if (_online.TryGetValue(userId, out var connection))
            {
                Enqueue(connection, frame, ref slow);
            }
        }
    }

    private Room MirrorLocked(RoomRecord record, IEnumerable<long> memberIds)
    {
        var room = new Room(record.Id, record.Kind, record.Name, record.CreatedBy);
        foreach (var userId in memberIds)
        {
            room.MemberIds.Add(userId);
        }

        _rooms[room.Id] = room;
        _order.Add(room);
        return room;
    }

    // general is first in _order, so one pass is PROTOCOL.md's order: general, then the rest of
    // the account's rooms in the order the mirror learned them.
    private void EnqueueRoomStates(ClientConnection connection, long userId, ref List<ClientConnection>? slow)
    {
        foreach (var room in _order)
        {
            if (Holds(room, connection, userId))
            {
                EnqueueState(connection, room, ref slow);
            }
        }
    }

    // A room is always described in full: PROTOCOL.md pairs every RoomState with the voice
    // occupancy of the same room, even when that occupancy is empty.
    private static void EnqueueState(ClientConnection connection, Room room, ref List<ClientConnection>? slow)
    {
        Enqueue(connection, new ServerFrame { RoomState = StateOf(room) }, ref slow);
        Enqueue(connection, VoiceStateOf(room), ref slow);
    }

    // The whole text room hears it, the leaver included. The media session itself is released
    // after the lock, because RemoveSession can raise SpeakingChanged straight back into a
    // broadcast.
    private void RemoveVoiceLocked(Room room, long userId, ref List<ClientConnection>? slow, List<uint> removed)
    {
        if (!room.Voice.TryGetValue(userId, out var slot))
        {
            return;
        }

        // Both sides of the share graph go before the session does: the room hears ShareStopped
        // ahead of VoiceMemberLeft, and whoever this slot was watching loses a watcher. The slot
        // is still in the map, so the counts are read after its own watch is cleared.
        if (slot.Sharing)
        {
            StopShareLocked(room, userId, slot, ref slow);
        }

        if (slot.Watching is { } watched)
        {
            slot.Watching = null;
            if (room.Voice.TryGetValue(watched, out var sharer))
            {
                Enqueue(sharer.Connection, ShareWatchersOf(room, watched), ref slow);
            }
        }

        room.Voice.Remove(userId);
        removed.Add(slot.Session.Ssrc);
        BroadcastLocked(
            room,
            new ServerFrame { VoiceMemberLeft = new VoiceMemberLeft { RoomId = room.Id, UserId = userId } },
            except: null,
            ref slow);
    }

    // Ends a share and tells everyone it concerns. The relay drops its own watcher list with the
    // flag, so no viewer has to be detached there one at a time.
    private void StopShareLocked(Room room, long userId, VoiceSlot slot, ref List<ClientConnection>? slow)
    {
        slot.Sharing = false;
        slot.ShareAudio = false;

        foreach (var (watcherId, watcher) in room.Voice)
        {
            if (watcherId != userId && watcher.Watching == userId)
            {
                watcher.Watching = null;
                Enqueue(watcher.Connection, WatchStateOf(room, 0), ref slow);
            }
        }

        _relay.SetSharing(slot.Session.Ssrc, false, false);
        BroadcastLocked(
            room,
            new ServerFrame { ShareStopped = new ShareStopped { RoomId = room.Id, UserId = userId } },
            except: null,
            ref slow);
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

    private static ServerFrame RoomListOf(IReadOnlyList<RoomEntryRecord> entries)
    {
        var list = new RoomList();
        foreach (var entry in entries)
        {
            var room = new Protocol.Room
            {
                RoomId = entry.Room.Id,
                Kind = (Protocol.RoomKind)entry.Room.Kind,
                Name = entry.Room.Name,
                CreatedBy = entry.Room.CreatedBy ?? 0,
            };

            room.MemberIds.AddRange(entry.MemberIds);
            list.Rooms.Add(new RoomEntry
            {
                Room = room,
                Unread = entry.Unread,
                Mentions = entry.Mentions,
                LastMessageId = entry.LastMessageId,
            });
        }

        return new ServerFrame { RoomList = list };
    }

    // The room's shared facts only: counters are per reader and never ride a RoomUpdated.
    private static ServerFrame RoomUpdatedOf(Room room)
    {
        var protocol = new Protocol.Room
        {
            RoomId = room.Id,
            Kind = (Protocol.RoomKind)room.Kind,
            Name = room.Name,
            CreatedBy = room.CreatedBy ?? 0,
        };

        protocol.MemberIds.AddRange(room.MemberIds.Order());
        return new ServerFrame { RoomUpdated = new RoomUpdated { Room = protocol } };
    }

    private static ServerFrame VoiceStateOf(Room room)
    {
        var state = new VoiceState { RoomId = room.Id };
        foreach (var slot in room.Voice.Values)
        {
            state.Members.Add(VoiceMemberOf(slot));
        }

        return new ServerFrame { VoiceState = state };
    }

    private static VoiceMember VoiceMemberOf(VoiceSlot slot)
        => new()
        {
            UserId = slot.Session.UserId,
            Username = slot.Connection.Username ?? string.Empty,
            Ssrc = slot.Session.Ssrc,
            Sharing = slot.Sharing,
            ShareAudio = slot.ShareAudio,
        };

    private static ServerFrame WatchStateOf(Room room, long sharerUserId)
        => new() { WatchState = new WatchState { RoomId = room.Id, UserId = sharerUserId } };

    // The registry's own map is the truth a client is told about: the relay counts the same
    // watchers, but only for diagnostics, and it lags this map by whatever is in flight.
    private static ServerFrame ShareWatchersOf(Room room, long sharerUserId)
        => new()
        {
            ShareWatchers = new ShareWatchers
            {
                RoomId = room.Id,
                Count = (uint)room.Voice.Values.Count(slot => slot.Watching == sharerUserId),
            },
        };

    // Only ever enqueued to the joiner: it carries that session's media key.
    private ServerFrame VoiceReadyOf(Room room, VoiceSession session)
        => new()
        {
            VoiceReady = new VoiceReady
            {
                RoomId = room.Id,
                Host = _relay.AdvertisedHost,
                Port = (uint)_relay.Port,
                Key = ByteString.CopyFrom(session.Key),
                Ssrc = session.Ssrc,
            },
        };

    private void ReleaseVoice(List<uint> removed)
    {
        foreach (var ssrc in removed)
        {
            _relay.RemoveSession(ssrc);
        }
    }

    // Raised from the relay's threads. No membership re-check: a false that lands after the
    // user left says nothing a client cannot already handle, and re-taking _gate here is what
    // keeps this frame ordered against the room's own voice membership changes.
    private void OnSpeakingChanged(string roomId, long userId, bool speaking)
        => BroadcastToRoom(
            roomId,
            new ServerFrame { Speaking = new Speaking { RoomId = roomId, UserId = userId, Speaking_ = speaking } });

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
            _logger.LogWarning(
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
            _logger.LogError(ex, "Connection {ConnectionId}: failed to close with {CloseCode}", connection.Id, (int)status);
        }
    }

    // One account's live voice session in one room. The connection is kept alongside the session
    // because VoiceMember carries the username, which lives on the connection and nowhere else.
    private sealed class VoiceSlot(ClientConnection connection, VoiceSession session)
    {
        public ClientConnection Connection { get; } = connection;

        public VoiceSession Session { get; } = session;

        public bool Sharing { get; set; }

        public bool ShareAudio { get; set; }

        // The sharer this session is watching, if any. A viewer watches at most one share, and
        // only inside its own room.
        public long? Watching { get; set; }
    }

    // One mirrored room: its persisted facts, the accounts that belong to it, and the live
    // connections of those of them that are online right now.
    private sealed class Room(string id, Data.RoomKind kind, string name, long? createdBy)
    {
        public string Id { get; } = id;

        public Data.RoomKind Kind { get; } = kind;

        public string Name { get; } = name;

        public long? CreatedBy { get; } = createdBy;

        public HashSet<long> MemberIds { get; } = [];

        public Dictionary<long, ClientConnection> Members { get; } = [];

        public Dictionary<long, VoiceSlot> Voice { get; } = [];
    }
}
