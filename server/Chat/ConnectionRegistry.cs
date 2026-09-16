using System.Collections.Concurrent;
using System.Diagnostics;
using System.Diagnostics.CodeAnalysis;
using System.Net.WebSockets;
using Google.Protobuf;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;
using Vorcall.Server.Voice;

namespace Vorcall.Server.Chat;

// UnknownChannel is id 0, an id nothing names, and a channel the caller may not view alike:
// PROTOCOL.md answers all three with ERROR_CODE_UNKNOWN_CHANNEL, so a hidden channel is
// indistinguishable from a missing one. NotLive is a connection the account has already replaced.
public enum JoinVoiceOutcome
{
    UnknownChannel,
    NotVoiceChannel,
    PermissionDenied,
    Unavailable,
    NotLive,
    Joined,
}

public enum LeaveVoiceOutcome
{
    UnknownChannel,
    NotInVoice,
    NotLive,
    Left,
}

public enum StartShareOutcome
{
    UnknownChannel,
    NotInVoice,
    PermissionDenied,

    // Sharing is switched off on this server, or the relay never came up.
    Unavailable,

    // The channel already holds as many sharers as the relay allows.
    Limit,
    NotLive,
    Started,
}

public enum StopShareOutcome
{
    UnknownChannel,
    NotInVoice,
    NotSharing,
    NotLive,
    Stopped,
}

public enum WatchShareOutcome
{
    UnknownChannel,
    NotInVoice,

    // The target is not in the channel, is not sharing, or is the caller itself.
    NotSharing,
    NotLive,
    Watching,
}

public enum UnwatchShareOutcome
{
    UnknownChannel,
    NotInVoice,
    NotLive,
    Unwatched,
}

public enum StartCameraOutcome
{
    UnknownChannel,
    NotInVoice,
    PermissionDenied,

    // Cameras are switched off on this server, or the relay never came up.
    Unavailable,

    // The channel already holds as many cameras as the relay allows.
    Limit,
    NotLive,
    Started,
}

public enum StopCameraOutcome
{
    UnknownChannel,
    NotInVoice,
    NotOnCamera,
    NotLive,
    Stopped,
}

public enum WatchCameraOutcome
{
    UnknownChannel,
    NotInVoice,

    // The target is not in the channel or has no camera on.
    NotOnCamera,

    // The target is the caller itself.
    Forbidden,

    // The caller already watches as many cameras as the relay allows.
    Limit,
    NotLive,
    Watching,
}

public enum UnwatchCameraOutcome
{
    UnknownChannel,
    NotInVoice,
    NotLive,
    Unwatched,
}

public enum VoiceSelfStateOutcome
{
    UnknownChannel,
    NotInVoice,
    NotLive,

    // Already what the caller says it is, so nothing was broadcast.
    Unchanged,
    Set,
}

// Where a frame addressed to one account went. SlowConsumer is a live connection that could not
// take it and is being closed for falling behind: for a caller that waits on an answer to the
// frame, the account is as good as offline.
public enum SendToOutcome
{
    Offline,
    SlowConsumer,
    Sent,
}

// Everything a scrape wants to know about the presence model, read in one pass so the figures
// describe the same moment rather than one moment each.
public readonly record struct RegistrySnapshot(
    int Connections,
    int OnlineUsers,
    int Channels,
    int VoiceSessions,
    int Sharers,
    int Watchers,
    int Cameras,
    int CameraWatchers);

// What a caller outside the registry needs to know about one channel without reaching into the
// model: its kind, where it sits, and a DM's two members.
public sealed record ChannelInfo(long Id, Data.ChannelKind Kind, string Name, long? CategoryId, long? DmLow, long? DmHigh);

// The verdict of an attach. Replaced is the connection this one took over from, which the caller
// fails with ERROR_CODE_SESSION_REPLACED outside the lock. Attached is false only when the account
// is not a member of the server, which the caller answers with a fatal error of its own — the two
// cases have to be told apart, and a null Replaced is the ordinary outcome of a first connection.
public sealed record AttachOutcome(bool Attached, ClientConnection? Replaced)
{
    public static AttachOutcome Unknown { get; } = new(false, null);

    public static AttachOutcome Ok(ClientConnection? replaced) => new(true, replaced);
}

// The connection set, the mirror of the persisted server, and presence, in one place. Every
// mutation and every broadcast runs under _gate, which is what gives PROTOCOL.md its ordering
// guarantee: a connection cannot see a ChatMessage, a voice frame or a delta before its own Welcome
// and ServerSnapshot, because those are enqueued under the same lock.
//
// The server row, roles, categories, channels, overrides and member profiles are persistent rows;
// this class holds a mirror of them and never touches the database except through the directories
// at startup. Every caller writes the row first and then tells the mirror.
//
// Visibility is the whole membership model: a member is "in" a channel exactly when VIEW_CHANNEL
// resolves for them in it. MemberState.Visible is the set each online client was told about, and
// it is the audience of every channel-scoped frame.
public sealed partial class ConnectionRegistry
{
    // The channel-scoped bits a DM's two members always hold, whatever the roles say: PROTOCOL.md
    // § Server, channels and categories has them always able to view it, send in it and connect to
    // its voice session.
    private const ulong DmGrants =
        (ulong)Perm.ViewChannel
        | (ulong)Perm.SendMessages
        | (ulong)Perm.AttachFiles
        | (ulong)Perm.AddReactions
        | (ulong)Perm.Connect
        | (ulong)Perm.Speak
        | (ulong)Perm.ShareScreen
        | (ulong)Perm.Video;

    private readonly Lock _gate = new();

    // Every accepted socket, whether or not it finished its handshake.
    private readonly ConcurrentDictionary<Guid, ClientConnection> _connections = new();

    // The mirror, all of it guarded by _gate.
    private readonly ServerFacts _facts = new();
    private readonly Dictionary<long, RoleRecord> _roleById = [];
    private readonly Dictionary<long, CategoryRecord> _categories = [];
    private readonly Dictionary<long, ChannelState> _channelById = [];

    // The soundpad library. Server-wide and unfiltered: every member sees every clip, so unlike a
    // channel there is nothing to resolve per reader.
    private readonly Dictionary<long, SoundRecord> _soundById = [];

    // The sticker library, server-wide and unfiltered the same way.
    private readonly Dictionary<long, StickerRecord> _stickerById = [];

    // Every account that is not banned, online or not.
    private readonly Dictionary<long, MemberState> _memberById = [];

    // The one live connection per account.
    private readonly Dictionary<long, ClientConnection> _online = [];

    private readonly ChannelDirectory _channels;
    private readonly RoleDirectory _roles;
    private readonly MemberDirectory _members;
    private readonly ServerDirectory _server;
    private readonly VoiceRelay _relay;
    private readonly StreamRegistry _streams;
    private readonly SoundStore _sounds;
    private readonly StickerStore _stickers;
    private readonly ILogger<ConnectionRegistry> _logger;

    private long _everyoneRoleId;

    // Rebuilt whenever a role or the owner changes; never null, so a question asked before
    // LoadAsync resolves to nothing rather than throwing.
    private Hierarchy _hierarchy = new(0, []);

    public ConnectionRegistry(
        ChannelDirectory channels,
        RoleDirectory roles,
        MemberDirectory members,
        ServerDirectory server,
        VoiceRelay relay,
        StreamRegistry streams,
        SoundStore sounds,
        StickerStore stickers,
        ILogger<ConnectionRegistry> logger)
    {
        _channels = channels;
        _roles = roles;
        _members = members;
        _server = server;
        _relay = relay;
        _streams = streams;
        _sounds = sounds;
        _stickers = stickers;
        _logger = logger;
        _relay.SpeakingChanged += OnSpeakingChanged;
    }

    public int Count => _connections.Count;

    public long? OwnerId
    {
        get
        {
            lock (_gate)
            {
                return _facts.OwnerId;
            }
        }
    }

    public long GeneralChannelId
    {
        get
        {
            lock (_gate)
            {
                return _facts.GeneralChannelId;
            }
        }
    }

    public long EveryoneRoleId
    {
        get
        {
            lock (_gate)
            {
                return _everyoneRoleId;
            }
        }
    }

    // One pass under the lock every mutation already takes: a scrape costs the mirror a single
    // traversal of the channels and never holds the gate across anything that can block.
    public RegistrySnapshot Snapshot()
    {
        lock (_gate)
        {
            var voiceSessions = 0;
            var sharers = 0;
            var watchers = 0;
            var cameras = 0;
            var cameraWatchers = 0;

            foreach (var channel in _channelById.Values)
            {
                voiceSessions += channel.Voice.Count;
                foreach (var slot in channel.Voice.Values)
                {
                    if (slot.Sharing)
                    {
                        sharers++;
                    }

                    if (slot.Watching is not null)
                    {
                        watchers++;
                    }

                    if (slot.Camera)
                    {
                        cameras++;
                    }

                    // One viewer of four cameras counts four times: the figure is cameras
                    // received, which is what the relay's fan-out costs.
                    cameraWatchers += slot.WatchingCameras.Count;
                }
            }

            return new RegistrySnapshot(
                _connections.Count,
                _online.Count,
                _channelById.Count,
                voiceSessions,
                sharers,
                watchers,
                cameras,
                cameraWatchers);
        }
    }

    public void Add(ClientConnection connection) => _connections[connection.Id] = connection;

    public void Remove(ClientConnection connection) => _connections.TryRemove(connection.Id, out _);

    // Called once at startup, before anything listens: a channel, role or member that is not in the
    // mirror cannot be seen, written to or resolved against, so the mirror has to be complete
    // before the first Hello. Refuses to run a second time while connections are live, because
    // replacing the mirror underneath them would strand their voice slots and their Visible sets.
    public async Task LoadAsync(CancellationToken ct)
    {
        var facts = await _server.LoadAsync(ct);
        var (roles, rolesByUser) = await _roles.LoadAllAsync(ct);
        var (categories, channels, overrides) = await _channels.LoadAllAsync(ct);
        var members = await _members.LoadAllAsync(ct);
        var sounds = await _sounds.ListAsync(ct);
        var stickers = await _stickers.ListAsync(ct);

        int roleCount;
        int categoryCount;
        int channelCount;
        int memberCount;
        lock (_gate)
        {
            if (_online.Count > 0)
            {
                throw new InvalidOperationException(
                    "The registry is already serving connections: LoadAsync runs once, before anything listens.");
            }

            var everyone = roles.Find(role => role.IsEveryone)
                ?? throw new InvalidOperationException("No @everyone role exists: the database was never seeded.");

            if (facts.GeneralChannelId is not { } generalId)
            {
                throw new InvalidOperationException("The server row names no general channel: the database was never seeded.");
            }

            var general = channels.Find(channel => channel.Id == generalId);
            if (general is null || general.Kind != Data.ChannelKind.Text)
            {
                throw new InvalidOperationException(
                    $"general_channel_id {generalId} names no text channel: the database is inconsistent.");
            }

            _facts.Name = facts.Name;
            _facts.Description = facts.Description;
            _facts.IconImageId = facts.IconImageId;
            _facts.OwnerId = facts.OwnerId;
            _facts.GeneralChannelId = generalId;
            _everyoneRoleId = everyone.Id;

            _roleById.Clear();
            foreach (var role in roles)
            {
                _roleById[role.Id] = role;
            }

            RebuildHierarchyLocked();

            _categories.Clear();
            foreach (var category in categories)
            {
                _categories[category.Id] = category;
            }

            _channelById.Clear();
            foreach (var channel in channels)
            {
                _channelById[channel.Id] = new ChannelState(channel);
            }

            foreach (var entry in overrides)
            {
                if (_channelById.TryGetValue(entry.ChannelId, out var channel))
                {
                    channel.Overrides[(entry.TargetKind, entry.TargetId)] = entry;
                }
            }

            foreach (var channel in _channelById.Values)
            {
                RebuildDefLocked(channel);
            }

            _memberById.Clear();
            foreach (var member in members)
            {
                var state = new MemberState(member);

                // A role id the member holds that names no role is dropped, and @everyone is never
                // stored on a member: it is held implicitly.
                if (rolesByUser.TryGetValue(member.UserId, out var held))
                {
                    foreach (var roleId in held)
                    {
                        if (_roleById.TryGetValue(roleId, out var role) && !role.IsEveryone)
                        {
                            state.Roles.Add(roleId);
                        }
                    }
                }

                _memberById[member.UserId] = state;
            }

            _soundById.Clear();
            foreach (var sound in sounds)
            {
                _soundById[sound.Id] = sound;
            }

            _stickerById.Clear();
            foreach (var sticker in stickers)
            {
                _stickerById[sticker.Id] = sticker;
            }

            roleCount = _roleById.Count;
            categoryCount = _categories.Count;
            channelCount = _channelById.Count;
            memberCount = _memberById.Count;
        }

        _logger.LogInformation(
            "Mirrored {RoleCount} roles, {CategoryCount} categories, {ChannelCount} channels, {MemberCount} members",
            roleCount,
            categoryCount,
            channelCount,
            memberCount);
    }

    // 0 for an account the server does not have — banned or never registered — and for a channel id
    // nothing names, so a caller that cannot be resolved can never pass a permission check.
    public ulong Resolve(long userId, long? channelId)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(userId, out var member))
            {
                return 0;
            }

            if (channelId is not { } id)
            {
                return ResolveLocked(member, null);
            }

            return _channelById.TryGetValue(id, out var channel) ? ResolveLocked(member, channel) : 0;
        }
    }

    public bool Has(long userId, long? channelId, Perm bit) => Perms.Has(Resolve(userId, channelId), bit);

    // Asked of an account, not of a connection: the history and attachment endpoints serve a member
    // whether or not it is online.
    public bool CanView(long userId, long channelId)
    {
        lock (_gate)
        {
            return _memberById.TryGetValue(userId, out var member)
                && _channelById.TryGetValue(channelId, out var channel)
                && CanViewLocked(member, channel);
        }
    }

    public ChannelInfo? ChannelOf(long channelId)
    {
        lock (_gate)
        {
            if (!_channelById.TryGetValue(channelId, out var channel))
            {
                return null;
            }

            var record = channel.Record;
            return new ChannelInfo(record.Id, record.Kind, record.Name, record.CategoryId, record.DmLow, record.DmHigh);
        }
    }

    // Whether a SendMessage may name this sticker: the mirror holds complete rows only.
    public bool HasSticker(long stickerId)
    {
        lock (_gate)
        {
            return _stickerById.ContainsKey(stickerId);
        }
    }

    public bool IsOwner(long userId)
    {
        lock (_gate)
        {
            return _facts.OwnerId is { } owner && owner == userId;
        }
    }

    public bool IsMember(long userId)
    {
        lock (_gate)
        {
            return _memberById.ContainsKey(userId);
        }
    }

    public bool IsOnline(long userId)
    {
        lock (_gate)
        {
            return _online.ContainsKey(userId);
        }
    }

    public ClientConnection? LiveConnection(long userId)
    {
        lock (_gate)
        {
            return _online.GetValueOrDefault(userId);
        }
    }

    public IReadOnlyList<Protocol.Profile> Profiles()
    {
        lock (_gate)
        {
            var profiles = new List<Protocol.Profile>(_memberById.Count);
            foreach (var member in _memberById.Values.OrderBy(state => state.Profile.UserId))
            {
                profiles.Add(ProfileOf(member));
            }

            return profiles;
        }
    }

    // Publishes the connection as the account's live one and queues its Welcome, ServerSnapshot and
    // VoiceState frames. Returns the connection it replaced, if any: the caller fails that one
    // outside the lock, because a dead socket must never hold up the account's new session.
    public async Task<AttachOutcome> AttachAsync(
        ClientConnection connection,
        long userId,
        string username,
        long latestMessageId,
        CancellationToken ct)
    {
        bool known;
        lock (_gate)
        {
            known = _memberById.ContainsKey(userId);
        }

        // An account that registered after LoadAsync and was never announced: nothing about it can
        // be resolved until the mirror has its profile, so it is announced here instead. The read
        // filters bans, because PROTOCOL.md § Moderation has a banned account out of the mirror for
        // good and this is the one path that would put it back — a ban that landed between the
        // upgrade's own check and this Hello.
        if (!known)
        {
            var record = await _members.GetIfNotBannedAsync(userId, ct);
            if (record is null)
            {
                return AttachOutcome.Unknown;
            }

            MemberRegistered(record);
        }

        List<long> readIds;
        lock (_gate)
        {
            if (!_memberById.TryGetValue(userId, out var member))
            {
                return AttachOutcome.Unknown;
            }

            readIds = ReadableIdsLocked(member);
        }

        // Outside the lock: the counters are a database read, and _gate is never held across an
        // await. Visibility is resolved again below, so a change that lands in between is no loss.
        var readStates = await _channels.ReadStatesForAsync(userId, readIds, ct);

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        ClientConnection? replaced;
        lock (_gate)
        {
            if (!_memberById.TryGetValue(userId, out var member))
            {
                return AttachOutcome.Unknown;
            }

            // Before the first Enqueue: a refused frame only counts as a slow consumer once the
            // connection is ready.
            connection.MarkReady(userId, username);

            _online.TryGetValue(userId, out replaced);
            if (replaced is not null && !ReferenceEquals(replaced, connection))
            {
                // The replaced connection's voice sessions end while it is still the live one, so
                // every VoiceMemberLeft lands on its outbox rather than the new connection's, which
                // owes Welcome its first frame. The media key and ssrc never transfer.
                RemoveVoiceEverywhereLocked(userId, ref slow, removed);
            }

            _online[userId] = connection;
            RecomputeVisibleLocked(member);

            var visible = VisibleChannelsLocked(member);
            Enqueue(
                connection,
                new ServerFrame
                {
                    Welcome = new Welcome { LatestMessageId = latestMessageId, MemberId = userId, Username = username },
                },
                ref slow);
            Enqueue(connection, SnapshotOf(visible, readStates.ToDictionary(state => state.ChannelId)), ref slow);

            // After the snapshot that describes the channels they name, and a channel with nobody
            // in voice is not announced at all.
            foreach (var channel in visible)
            {
                if (channel.Voice.Count > 0)
                {
                    Enqueue(connection, VoiceStateOf(channel), ref slow);
                }
            }

            // A replacement is silent: the account never stopped being online, so nobody is told it
            // arrived.
            if (replaced is null)
            {
                BroadcastToAllLocked(MemberUpdatedOf(member), except: userId, ref slow);
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return AttachOutcome.Ok(ReferenceEquals(replaced, connection) ? null : replaced);
    }

    // Announces the end of a session. A connection that was already replaced is not the account's
    // live one and announces nothing: its successor owns the account's presence.
    public void Detach(ClientConnection connection)
    {
        if (connection.UserId is not { } userId)
        {
            return;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        var wentOffline = false;
        lock (_gate)
        {
            if (!IsLive(connection, userId))
            {
                // The ssrc belongs to this socket and to nothing else, so a slot it somehow still
                // holds is released even though the account has moved on.
                RemoveVoiceOfConnectionLocked(connection, userId, ref slow, removed);
            }
            else
            {
                // VoiceMemberLeft before the presence frame, and while this connection is still a
                // viewer of the channels it was in.
                RemoveVoiceEverywhereLocked(userId, ref slow, removed);
                _online.Remove(userId);
                wentOffline = true;

                if (_memberById.TryGetValue(userId, out var member))
                {
                    // Visible is maintained for online members only; it is rebuilt at the next
                    // attach from the model as it stands then.
                    member.Visible.Clear();
                    BroadcastToAllLocked(MemberUpdatedOf(member), except: userId, ref slow);
                }
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);

        // Only when the account itself went offline: a replaced connection's successor may still
        // answer a StreamRequest that reached the old one, so its readers keep waiting.
        if (wentOffline)
        {
            _streams.FaultOwner(userId);
        }
    }

    // Called by the registration endpoint once the account exists: every online member learns about
    // it at once, offline, and the mirror can resolve its permissions from here on.
    public void MemberRegistered(MemberRecord member)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_memberById.TryGetValue(member.UserId, out var state))
            {
                state.Profile = member;
            }
            else
            {
                state = new MemberState(member);
                _memberById[member.UserId] = state;
            }

            BroadcastToAllLocked(MemberUpdatedOf(state), except: null, ref slow);
        }

        CloseSlow(slow);
    }

    // Every member who may view the channel, whether or not they are in its voice session.
    public void BroadcastToChannel(long channelId, ServerFrame frame)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                BroadcastToChannelLocked(channel, frame, except: null, ref slow);
            }
        }

        CloseSlow(slow);
    }

    public void BroadcastToAll(ServerFrame frame)
    {
        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            BroadcastToAllLocked(frame, except: null, ref slow);
        }

        CloseSlow(slow);
    }

    // Resolved by id under the gate at the moment of sending, never from a connection the caller
    // held: an attach replaces the account's live connection with a new object, so one captured
    // earlier can be the replaced one while the account is fully online elsewhere.
    public SendToOutcome SendTo(long userId, ServerFrame frame)
    {
        List<ClientConnection>? slow = null;
        SendToOutcome outcome;
        lock (_gate)
        {
            // Enqueue's rule, spelled out because the caller needs the verdict: a refusal with
            // IsReady set is a full outbox, one without is a close already latched, whose frames
            // are dropped and whose account is on its way offline.
            if (!_online.TryGetValue(userId, out var connection))
            {
                outcome = SendToOutcome.Offline;
            }
            else if (connection.TryEnqueue(frame))
            {
                outcome = SendToOutcome.Sent;
            }
            else if (connection.IsReady)
            {
                (slow ??= []).Add(connection);
                outcome = SendToOutcome.SlowConsumer;
            }
            else
            {
                outcome = SendToOutcome.Offline;
            }
        }

        CloseSlow(slow);
        return outcome;
    }

    public JoinVoiceOutcome JoinVoice(ClientConnection connection, long channelId, bool selfMuted, bool selfDeafened)
    {
        if (connection.UserId is not { } userId)
        {
            return JoinVoiceOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return JoinVoiceOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return JoinVoiceOutcome.UnknownChannel;
            }

            if (channel.Record.Kind == Data.ChannelKind.Text)
            {
                return JoinVoiceOutcome.NotVoiceChannel;
            }

            var permissions = ResolveLocked(member, channel);
            if (!Perms.Has(permissions, Perm.Connect))
            {
                return JoinVoiceOutcome.PermissionDenied;
            }

            if (!_relay.IsAvailable)
            {
                return JoinVoiceOutcome.Unavailable;
            }

            // An account holds one voice session at a time, and a rejoin is never a no-op: a client
            // that asks again has rebuilt its media engine with the sequence counter back at zero,
            // so handing back the same key would repeat (key, nonce) pairs the relay's replay
            // window already refuses. Every VoiceReady carries a pair that was never used before.
            RemoveVoiceEverywhereLocked(userId, ref slow, removed);

            // A member without SPEAK joins muted rather than being refused, and the flags are on
            // the session from its first packet: PROTOCOL.md § Moderation.
            var muted = member.Profile.ServerMuted || !Perms.Has(permissions, Perm.Speak);
            var deafened = member.Profile.ServerDeafened;
            var priority = Perms.Has(permissions, Perm.PrioritySpeaker);

            // CreateSession takes the relay's own lock and calls nothing back into here.
            var session = _relay.CreateSession(channelId, userId, muted, deafened, priority);
            var slot = new VoiceSlot(connection, session, muted, deafened, priority)
            {
                // Display state the joiner brought with it: a client that rejoins muted must not
                // flash as unmuted while its first VoiceSelfState is in flight.
                SelfMuted = selfMuted,
                SelfDeafened = selfDeafened,
            };
            channel.Voice[userId] = slot;

            Enqueue(connection, VoiceReadyOf(channel, session), ref slow);
            Enqueue(connection, VoiceStateOf(channel), ref slow);
            BroadcastToChannelLocked(
                channel,
                new ServerFrame
                {
                    VoiceMemberJoined = new VoiceMemberJoined { ChannelId = channelId, Member = VoiceMemberOf(slot) },
                },
                except: userId,
                ref slow);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return JoinVoiceOutcome.Joined;
    }

    public LeaveVoiceOutcome LeaveVoice(ClientConnection connection, long channelId)
    {
        if (connection.UserId is not { } userId)
        {
            return LeaveVoiceOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return LeaveVoiceOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return LeaveVoiceOutcome.UnknownChannel;
            }

            if (!channel.Voice.ContainsKey(userId))
            {
                return LeaveVoiceOutcome.NotInVoice;
            }

            RemoveVoiceLocked(channel, userId, ref slow, removed);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        return LeaveVoiceOutcome.Left;
    }

    // The caller's own mute and deafen switches, which the other clients draw and nothing else
    // reads: they never reach the relay, so a self-mute is never a moderator's to hold. There is no
    // permission to check either — this is the caller describing itself.
    public VoiceSelfStateOutcome SetVoiceSelfState(ClientConnection connection, long channelId, bool muted, bool deafened)
    {
        if (connection.UserId is not { } userId)
        {
            return VoiceSelfStateOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return VoiceSelfStateOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return VoiceSelfStateOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var slot))
            {
                return VoiceSelfStateOutcome.NotInVoice;
            }

            // A client that re-asserts what it already said must not make the server fan a
            // VoiceState out to the whole channel for nothing.
            if (slot.SelfMuted == muted && slot.SelfDeafened == deafened)
            {
                return VoiceSelfStateOutcome.Unchanged;
            }

            slot.SelfMuted = muted;
            slot.SelfDeafened = deafened;

            BroadcastToChannelLocked(channel, VoiceStateOf(channel), except: null, ref slow);
        }

        CloseSlow(slow);
        return VoiceSelfStateOutcome.Set;
    }

    // A share rides an existing voice session: the media travels on the ssrc the caller already
    // proved, so there is nothing to hand out here. Repeating StartShare is how a client
    // re-announces a share it is already running, which is why it neither counts against the
    // channel's ceiling nor restarts anything on the relay.
    public StartShareOutcome StartShare(ClientConnection connection, long channelId, bool audio)
    {
        if (connection.UserId is not { } userId)
        {
            return StartShareOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return StartShareOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return StartShareOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var slot))
            {
                return StartShareOutcome.NotInVoice;
            }

            if (!Perms.Has(ResolveLocked(member, channel), Perm.ShareScreen))
            {
                return StartShareOutcome.PermissionDenied;
            }

            if (!_relay.ShareEnabled)
            {
                return StartShareOutcome.Unavailable;
            }

            if (!slot.Sharing && channel.Voice.Values.Count(other => other.Sharing) >= _relay.MaxSharersPerRoom)
            {
                return StartShareOutcome.Limit;
            }

            slot.Sharing = true;
            slot.ShareAudio = audio;

            // SetSharing takes the relay's own lock and calls nothing back into here.
            _relay.SetSharing(slot.Session.Ssrc, true, audio);

            BroadcastToChannelLocked(
                channel,
                new ServerFrame { ShareStarted = new ShareStarted { ChannelId = channelId, UserId = userId, Audio = audio } },
                except: null,
                ref slow);

            // Watchers of a share that was already running keep their watch, so the count answered
            // here is the real one rather than zero.
            Enqueue(connection, ShareWatchersOf(channel, userId), ref slow);
        }

        CloseSlow(slow);
        return StartShareOutcome.Started;
    }

    public StopShareOutcome StopShare(ClientConnection connection, long channelId)
    {
        if (connection.UserId is not { } userId)
        {
            return StopShareOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return StopShareOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return StopShareOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var slot))
            {
                return StopShareOutcome.NotInVoice;
            }

            if (!slot.Sharing)
            {
                return StopShareOutcome.NotSharing;
            }

            StopShareLocked(channel, userId, slot, ref slow);
        }

        CloseSlow(slow);
        return StopShareOutcome.Stopped;
    }

    // A viewer watches at most one share at a time, so this is a replacement: the share it leaves
    // loses a watcher and hears about it, and the share it joins gains one.
    public WatchShareOutcome WatchShare(ClientConnection connection, long channelId, long targetUserId)
    {
        if (connection.UserId is not { } userId)
        {
            return WatchShareOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return WatchShareOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return WatchShareOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var viewer))
            {
                return WatchShareOutcome.NotInVoice;
            }

            // Watching yourself is refused the same way as watching a stranger: the relay would
            // only clear the watch, and the client must be told nothing is coming.
            if (targetUserId == userId
                || !channel.Voice.TryGetValue(targetUserId, out var target)
                || !target.Sharing)
            {
                return WatchShareOutcome.NotSharing;
            }

            if (viewer.Watching == targetUserId)
            {
                // Idempotent: nothing changed, so only the viewer's own answer is repeated and the
                // sharer is not told its count again.
                Enqueue(connection, WatchStateOf(channel, targetUserId), ref slow);
            }
            else
            {
                if (viewer.Watching is { } previous)
                {
                    // Cleared before the count is read, so the previous sharer hears the figure
                    // that is true after this viewer left it.
                    viewer.Watching = null;
                    if (channel.Voice.TryGetValue(previous, out var previousSharer))
                    {
                        Enqueue(previousSharer.Connection, ShareWatchersOf(channel, previous), ref slow);
                    }
                }

                viewer.Watching = targetUserId;
                _relay.Watch(viewer.Session.Ssrc, target.Session.Ssrc);
                Enqueue(connection, WatchStateOf(channel, targetUserId), ref slow);
                Enqueue(target.Connection, ShareWatchersOf(channel, targetUserId), ref slow);
            }
        }

        CloseSlow(slow);
        return WatchShareOutcome.Watching;
    }

    public UnwatchShareOutcome UnwatchShare(ClientConnection connection, long channelId)
    {
        if (connection.UserId is not { } userId)
        {
            return UnwatchShareOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return UnwatchShareOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return UnwatchShareOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var viewer))
            {
                return UnwatchShareOutcome.NotInVoice;
            }

            if (viewer.Watching is { } previous)
            {
                viewer.Watching = null;
                _relay.Watch(viewer.Session.Ssrc, null);
                if (channel.Voice.TryGetValue(previous, out var sharer))
                {
                    Enqueue(sharer.Connection, ShareWatchersOf(channel, previous), ref slow);
                }
            }

            // Answered even when nothing was being watched, so a client that believed otherwise is
            // corrected either way.
            Enqueue(connection, WatchStateOf(channel, 0), ref slow);
        }

        CloseSlow(slow);
        return UnwatchShareOutcome.Unwatched;
    }

    // A camera rides an existing voice session exactly as a share does, and independently of one:
    // the same member may share a screen and be on camera at once. Repeating StartCamera is how a
    // client re-announces a camera it is already running, which is why it neither counts against
    // the channel's ceiling nor restarts anything on the relay.
    public StartCameraOutcome StartCamera(ClientConnection connection, long channelId)
    {
        if (connection.UserId is not { } userId)
        {
            return StartCameraOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return StartCameraOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return StartCameraOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var slot))
            {
                return StartCameraOutcome.NotInVoice;
            }

            if (!Perms.Has(ResolveLocked(member, channel), Perm.Video))
            {
                return StartCameraOutcome.PermissionDenied;
            }

            if (!_relay.CameraEnabled)
            {
                return StartCameraOutcome.Unavailable;
            }

            if (!slot.Camera && channel.Voice.Values.Count(other => other.Camera) >= _relay.MaxCamerasPerRoom)
            {
                return StartCameraOutcome.Limit;
            }

            slot.Camera = true;

            // SetCamera takes the relay's own lock and calls nothing back into here.
            _relay.SetCamera(slot.Session.Ssrc, true);

            BroadcastToChannelLocked(
                channel,
                new ServerFrame { CameraStarted = new CameraStarted { ChannelId = channelId, UserId = userId } },
                except: null,
                ref slow);

            // Watchers of a camera that was already running keep their watch, so the count
            // answered here is the real one rather than zero.
            Enqueue(connection, CameraWatchersOf(channel, userId), ref slow);
        }

        CloseSlow(slow);
        return StartCameraOutcome.Started;
    }

    public StopCameraOutcome StopCamera(ClientConnection connection, long channelId)
    {
        if (connection.UserId is not { } userId)
        {
            return StopCameraOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return StopCameraOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return StopCameraOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var slot))
            {
                return StopCameraOutcome.NotInVoice;
            }

            if (!slot.Camera)
            {
                return StopCameraOutcome.NotOnCamera;
            }

            StopCameraLocked(channel, userId, slot, ref slow);
        }

        CloseSlow(slow);
        return StopCameraOutcome.Stopped;
    }

    // A viewer watches up to MaxWatchedCameras at once, so this adds to a set rather than
    // replacing one: the camera it joins gains a watcher and hears about it.
    public WatchCameraOutcome WatchCamera(ClientConnection connection, long channelId, long targetUserId)
    {
        if (connection.UserId is not { } userId)
        {
            return WatchCameraOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return WatchCameraOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return WatchCameraOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var viewer))
            {
                return WatchCameraOutcome.NotInVoice;
            }

            // A client draws its own camera from its own capture, so watching it over the relay is
            // refused rather than quietly accepted.
            if (targetUserId == userId)
            {
                return WatchCameraOutcome.Forbidden;
            }

            if (!channel.Voice.TryGetValue(targetUserId, out var target) || !target.Camera)
            {
                return WatchCameraOutcome.NotOnCamera;
            }

            if (viewer.WatchingCameras.Contains(targetUserId))
            {
                // Idempotent: nothing changed, so only the viewer's own answer is repeated and the
                // camera's owner is not told its count again.
                Enqueue(connection, CameraWatchStateOf(channel, viewer), ref slow);
            }
            else
            {
                if (viewer.WatchingCameras.Count >= _relay.MaxWatchedCameras)
                {
                    return WatchCameraOutcome.Limit;
                }

                viewer.WatchingCameras.Add(targetUserId);
                _relay.WatchCamera(viewer.Session.Ssrc, target.Session.Ssrc, watch: true);
                Enqueue(connection, CameraWatchStateOf(channel, viewer), ref slow);
                Enqueue(target.Connection, CameraWatchersOf(channel, targetUserId), ref slow);
            }
        }

        CloseSlow(slow);
        return WatchCameraOutcome.Watching;
    }

    // targetUserId 0 drops every camera the caller watches; anything else drops that one. Either
    // way the caller is answered with the set it holds afterwards.
    public UnwatchCameraOutcome UnwatchCamera(ClientConnection connection, long channelId, long targetUserId)
    {
        if (connection.UserId is not { } userId)
        {
            return UnwatchCameraOutcome.NotLive;
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!IsLive(connection, userId) || !_memberById.TryGetValue(userId, out var member))
            {
                return UnwatchCameraOutcome.NotLive;
            }

            if (!VisibleLocked(member, channelId, out var channel))
            {
                return UnwatchCameraOutcome.UnknownChannel;
            }

            if (!channel.Voice.TryGetValue(userId, out var viewer))
            {
                return UnwatchCameraOutcome.NotInVoice;
            }

            var dropped = new List<long>();
            if (targetUserId == 0)
            {
                dropped.AddRange(viewer.WatchingCameras);
            }
            else if (viewer.WatchingCameras.Contains(targetUserId))
            {
                dropped.Add(targetUserId);
            }

            // Cleared before the counts are read, so every owner hears the figure that is true
            // after this viewer left it.
            viewer.WatchingCameras.ExceptWith(dropped);

            foreach (var ownerId in dropped)
            {
                if (channel.Voice.TryGetValue(ownerId, out var owner))
                {
                    _relay.WatchCamera(viewer.Session.Ssrc, owner.Session.Ssrc, watch: false);
                    Enqueue(owner.Connection, CameraWatchersOf(channel, ownerId), ref slow);
                }
            }

            // Answered even when nothing was being watched, so a client that believed otherwise is
            // corrected either way.
            Enqueue(connection, CameraWatchStateOf(channel, viewer), ref slow);
        }

        CloseSlow(slow);
        return UnwatchCameraOutcome.Unwatched;
    }

    // Re-resolves visibility, voice CONNECT and the three voice flags after a permission change and
    // queues the resulting deltas. channelIds null is every channel, userIds null every online
    // member.
    public void Reevaluate(IEnumerable<long>? channelIds = null, IEnumerable<long>? userIds = null)
    {
        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            ReevaluateLocked(channelIds, userIds, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
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

    // See PROTOCOL.md § Roles and permissions for the ten steps; the engine owns them and this only
    // feeds it from the mirror. A DM is the one channel whose visibility is membership rather than
    // resolution: its two members always hold the channel-scoped bits of DmGrants, and nobody else
    // — the owner included — is given sight of it.
    private ulong ResolveLocked(MemberState member, ChannelState? channel)
    {
        var def = DefOf(member);
        if (channel is null)
        {
            return PermissionEngine.Resolve(_hierarchy, def, null);
        }

        if (!channel.IsDm)
        {
            return PermissionEngine.Resolve(_hierarchy, def, channel.Def);
        }

        var baseBits = PermissionEngine.Resolve(_hierarchy, def, null);
        return channel.IsDmMember(member.Profile.UserId)
            ? baseBits | DmGrants
            : baseBits & Perms.ServerScoped;
    }

    private bool CanViewLocked(MemberState member, ChannelState channel)
        => Perms.Has(ResolveLocked(member, channel), Perm.ViewChannel);

    // The channel ids the member may see right now. Recomputed from scratch rather than patched,
    // because a role change can move many channels at once.
    private void RecomputeVisibleLocked(MemberState member)
    {
        member.Visible.Clear();
        foreach (var channel in _channelById.Values)
        {
            if (CanViewLocked(member, channel))
            {
                member.Visible.Add(channel.Record.Id);
            }
        }
    }

    // The snapshot's read_states: every text channel the member may view, plus its DMs. A voice
    // channel holds no messages and therefore no counters.
    private List<long> ReadableIdsLocked(MemberState member)
    {
        var ids = new List<long>();
        foreach (var channel in _channelById.Values)
        {
            if (channel.Record.Kind != Data.ChannelKind.Voice && CanViewLocked(member, channel))
            {
                ids.Add(channel.Record.Id);
            }
        }

        ids.Sort();
        return ids;
    }

    private bool VisibleLocked(MemberState member, long channelId, [NotNullWhen(true)] out ChannelState? channel)
    {
        channel = null;
        if (!member.Visible.Contains(channelId) || !_channelById.TryGetValue(channelId, out var found))
        {
            return false;
        }

        channel = found;
        return true;
    }

    // The channels the member may see, in the order PROTOCOL.md § Hello sequence sends them: the
    // ones above every category first, then category by category.
    private List<ChannelState> VisibleChannelsLocked(MemberState member)
    {
        var visible = new List<ChannelState>(member.Visible.Count);
        foreach (var channelId in member.Visible)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                visible.Add(channel);
            }
        }

        visible.Sort(CompareChannelsLocked);
        return visible;
    }

    private int CompareChannelsLocked(ChannelState left, ChannelState right)
    {
        var byCategory = CategoryRankLocked(left.Record.CategoryId).CompareTo(CategoryRankLocked(right.Record.CategoryId));
        if (byCategory != 0)
        {
            return byCategory;
        }

        var byPosition = left.Record.Position.CompareTo(right.Record.Position);
        return byPosition != 0 ? byPosition : left.Record.Id.CompareTo(right.Record.Id);
    }

    // No category — a DM included — sorts above every category, which is where the wire's
    // category_id 0 puts it.
    private (int Position, long Id) CategoryRankLocked(long? categoryId)
        => categoryId is { } id && _categories.TryGetValue(id, out var category)
            ? (category.Position, category.Id)
            : (-1, 0);

    private void RebuildHierarchyLocked()
        => _hierarchy = new Hierarchy(
            // 0 matches no account: ids start at 1, so a server with no owner has none.
            _facts.OwnerId ?? 0,
            _roleById.Values.Select(role => new RoleDef(role.Id, role.Position, role.Permissions, role.IsEveryone)).ToList());

    private void RebuildDefLocked(ChannelState channel)
        => channel.Def = new ChannelDef(
            channel.Record.Id,
            channel.Record.Id == _facts.GeneralChannelId,
            channel.Overrides.Values
                .Select(entry => new OverrideDef(
                    entry.TargetKind == Data.OverrideTarget.Role ? entry.TargetId : null,
                    entry.TargetKind == Data.OverrideTarget.User ? entry.TargetId : null,
                    entry.Allow,
                    entry.Deny))
                .ToList());

    private static MemberDef DefOf(MemberState member) => new(member.Profile.UserId, member.Roles);

    // Visible is the audience of every channel-scoped frame: it is exactly what each client was
    // told it can see, so a frame about a channel reaches the members holding it and nobody else. A
    // DM is in the Visible set of its two members alone.
    private void BroadcastToChannelLocked(
        ChannelState channel,
        ServerFrame frame,
        long? except,
        ref List<ClientConnection>? slow)
    {
        var channelId = channel.Record.Id;
        foreach (var (userId, connection) in _online)
        {
            if (userId != except
                && _memberById.TryGetValue(userId, out var member)
                && member.Visible.Contains(channelId))
            {
                Enqueue(connection, frame, ref slow);
            }
        }
    }

    // Server facts, roles and categories concern every session, whatever it may see.
    private void BroadcastToAllLocked(ServerFrame frame, long? except, ref List<ClientConnection>? slow)
    {
        foreach (var (userId, connection) in _online)
        {
            if (userId != except)
            {
                Enqueue(connection, frame, ref slow);
            }
        }
    }

    private void SendToLocked(long userId, ServerFrame frame, ref List<ClientConnection>? slow)
    {
        if (_online.TryGetValue(userId, out var connection))
        {
            Enqueue(connection, frame, ref slow);
        }
    }

    // Never awaits a socket: a connection that cannot take the frame is collected and closed after
    // the lock is released, so one slow client cannot stall a channel.
    private static void Enqueue(ClientConnection connection, ServerFrame frame, ref List<ClientConnection>? slow)
    {
        // CloseAsync clears IsReady before it stops accepting frames, so a refusal that comes with
        // IsReady still set is a full outbox and nothing else.
        if (connection.TryEnqueue(frame) || !connection.IsReady)
        {
            return;
        }

        (slow ??= []).Add(connection);
    }

    // The one place a voice slot ends. The whole channel hears it, the leaver included unless the
    // caller says otherwise; the media session itself is released after the lock, because
    // RemoveSession can raise SpeakingChanged straight back into a broadcast.
    private void RemoveVoiceLocked(
        ChannelState channel,
        long userId,
        ref List<ClientConnection>? slow,
        List<uint> removed,
        bool notifyLeaver = true)
    {
        if (!channel.Voice.TryGetValue(userId, out var slot))
        {
            return;
        }

        // Both sides of the share graph go before the session does: the channel hears ShareStopped
        // ahead of VoiceMemberLeft, and whoever this slot was watching loses a watcher. The slot is
        // still in the map, so the counts are read after its own watch is cleared.
        if (slot.Sharing)
        {
            StopShareLocked(channel, userId, slot, ref slow);
        }

        if (slot.Watching is { } watched)
        {
            slot.Watching = null;
            if (channel.Voice.TryGetValue(watched, out var sharer))
            {
                Enqueue(sharer.Connection, ShareWatchersOf(channel, watched), ref slow);
            }
        }

        // The camera graph goes the same way, and just as independently of the share.
        if (slot.Camera)
        {
            StopCameraLocked(channel, userId, slot, ref slow);
        }

        if (slot.WatchingCameras.Count > 0)
        {
            var watchedCameras = slot.WatchingCameras.ToList();
            slot.WatchingCameras.Clear();
            foreach (var ownerId in watchedCameras)
            {
                if (channel.Voice.TryGetValue(ownerId, out var owner))
                {
                    Enqueue(owner.Connection, CameraWatchersOf(channel, ownerId), ref slow);
                }
            }
        }

        channel.Voice.Remove(userId);
        removed.Add(slot.Session.Ssrc);

        // Nobody is left to hear it, and the record would otherwise outlive the session it was
        // played into: the next joiner must not inherit a clip from a previous one.
        if (channel.Voice.Count == 0)
        {
            channel.Playing = null;
        }

        BroadcastToChannelLocked(
            channel,
            new ServerFrame { VoiceMemberLeft = new VoiceMemberLeft { ChannelId = channel.Record.Id, UserId = userId } },
            except: notifyLeaver ? null : userId,
            ref slow);
    }

    // An account holds one voice session at a time, but the slot is found by scanning: the mirror
    // keeps no second index of it, and a join, a replacement or a disconnect has to be sure it left
    // none behind.
    private void RemoveVoiceEverywhereLocked(long userId, ref List<ClientConnection>? slow, List<uint> removed)
    {
        foreach (var channel in _channelById.Values)
        {
            if (channel.Voice.ContainsKey(userId))
            {
                RemoveVoiceLocked(channel, userId, ref slow, removed);
            }
        }
    }

    private void RemoveVoiceOfConnectionLocked(
        ClientConnection connection,
        long userId,
        ref List<ClientConnection>? slow,
        List<uint> removed)
    {
        foreach (var channel in _channelById.Values)
        {
            if (channel.Voice.TryGetValue(userId, out var slot) && ReferenceEquals(slot.Connection, connection))
            {
                RemoveVoiceLocked(channel, userId, ref slow, removed);
            }
        }
    }

    // Ends a share and tells everyone it concerns. The relay drops its own watcher list with the
    // flag, so no viewer has to be detached there one at a time.
    private void StopShareLocked(ChannelState channel, long userId, VoiceSlot slot, ref List<ClientConnection>? slow)
    {
        slot.Sharing = false;
        slot.ShareAudio = false;

        foreach (var (watcherId, watcher) in channel.Voice)
        {
            if (watcherId != userId && watcher.Watching == userId)
            {
                watcher.Watching = null;
                Enqueue(watcher.Connection, WatchStateOf(channel, 0), ref slow);
            }
        }

        _relay.SetSharing(slot.Session.Ssrc, false, false);
        BroadcastToChannelLocked(
            channel,
            new ServerFrame { ShareStopped = new ShareStopped { ChannelId = channel.Record.Id, UserId = userId } },
            except: null,
            ref slow);
    }

    // Ends a camera and tells everyone it concerns, exactly as StopShareLocked does for a share.
    // The relay drops its own watcher list with the flag, so no viewer has to be detached there one
    // at a time.
    private void StopCameraLocked(ChannelState channel, long userId, VoiceSlot slot, ref List<ClientConnection>? slow)
    {
        slot.Camera = false;

        foreach (var (watcherId, watcher) in channel.Voice)
        {
            if (watcherId != userId && watcher.WatchingCameras.Remove(userId))
            {
                Enqueue(watcher.Connection, CameraWatchStateOf(channel, watcher), ref slow);
            }
        }

        _relay.SetCamera(slot.Session.Ssrc, false);
        BroadcastToChannelLocked(
            channel,
            new ServerFrame { CameraStopped = new CameraStopped { ChannelId = channel.Record.Id, UserId = userId } },
            except: null,
            ref slow);
    }

    // PROTOCOL.md § Roles and permissions: any change to roles, overrides, channels or memberships
    // is re-resolved immediately. A member that gained a channel is sent it, one that lost it is
    // told the channel is gone, a voice session that lost VIEW_CHANNEL or CONNECT ends, and the
    // three moderation flags of a session that kept it are recomputed.
    //
    // A DM joins the flag pass but not the visibility one: its audience is its two members and
    // never changes, so it is never upserted or deleted here, while PRIORITY_SPEAKER resolves from
    // the member's roles and so moves in a DM exactly as it does in a voice channel.
    //
    // silenced collects the sessions whose talk spurt the new mute ended, for the caller to
    // announce once it has let go of _gate.
    private void ReevaluateLocked(
        IEnumerable<long>? channelIds,
        IEnumerable<long>? userIds,
        ref List<ClientConnection>? slow,
        List<uint> removed,
        List<(long ChannelId, long UserId)> silenced)
    {
        var channels = new List<ChannelState>();
        if (channelIds is null)
        {
            channels.AddRange(_channelById.Values);
        }
        else
        {
            foreach (var channelId in channelIds)
            {
                if (_channelById.TryGetValue(channelId, out var channel))
                {
                    channels.Add(channel);
                }
            }
        }

        var members = new List<MemberState>();
        foreach (var userId in userIds ?? _online.Keys)
        {
            if (_online.ContainsKey(userId) && _memberById.TryGetValue(userId, out var member))
            {
                members.Add(member);
            }
        }

        if (channels.Count == 0 || members.Count == 0)
        {
            return;
        }

        // One VoiceState per channel whose occupancy changed, however many members moved in it.
        var restate = new HashSet<long>();
        foreach (var member in members)
        {
            var userId = member.Profile.UserId;
            foreach (var channel in channels)
            {
                var channelId = channel.Record.Id;
                var permissions = ResolveLocked(member, channel);

                if (!channel.IsDm)
                {
                    var visible = Perms.Has(permissions, Perm.ViewChannel);
                    var wasVisible = member.Visible.Contains(channelId);

                    if (!visible)
                    {
                        if (!wasVisible)
                        {
                            continue;
                        }

                        member.Visible.Remove(channelId);
                        SendToLocked(userId, ChannelDeletedOf(channelId), ref slow);
                        if (channel.Voice.ContainsKey(userId))
                        {
                            RemoveVoiceLocked(channel, userId, ref slow, removed);
                            SendToLocked(userId, VoiceMovedOf(0), ref slow);
                        }

                        continue;
                    }

                    if (!wasVisible)
                    {
                        // Added before the frame goes out, so anything broadcast after this counts
                        // the member among the channel's viewers.
                        member.Visible.Add(channelId);
                        SendToLocked(userId, ChannelUpsertedOf(channel), ref slow);

                        // A channel it could not see until now may already have people in voice.
                        if (channel.Voice.Count > 0)
                        {
                            SendToLocked(userId, VoiceStateOf(channel), ref slow);
                        }

                        continue;
                    }
                }

                if (!channel.Voice.TryGetValue(userId, out var slot))
                {
                    continue;
                }

                if (!Perms.Has(permissions, Perm.Connect))
                {
                    RemoveVoiceLocked(channel, userId, ref slow, removed);
                    SendToLocked(userId, VoiceMovedOf(0), ref slow);
                    continue;
                }

                var muted = member.Profile.ServerMuted || !Perms.Has(permissions, Perm.Speak);
                var deafened = member.Profile.ServerDeafened;
                var priority = Perms.Has(permissions, Perm.PrioritySpeaker);
                if (slot.Muted == muted && slot.Deafened == deafened && slot.Priority == priority)
                {
                    continue;
                }

                slot.Muted = muted;
                slot.Deafened = deafened;
                slot.Priority = priority;

                // SetModeration takes only the relay's own lock and returns whether the mute ended a
                // talk spurt; the Speaking frame for it goes out once this call has released _gate.
                if (_relay.SetModeration(slot.Session.Ssrc, muted, deafened, priority))
                {
                    silenced.Add((channelId, userId));
                }

                restate.Add(channelId);
            }
        }

        foreach (var channelId in restate)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                BroadcastToChannelLocked(channel, VoiceStateOf(channel), except: null, ref slow);
            }
        }
    }

    private ServerFrame SnapshotOf(List<ChannelState> visible, Dictionary<long, ReadStateRecord> readStates)
    {
        var snapshot = new ServerSnapshot { Server = ServerOf() };

        // Every role and every category, whatever the reader may view: a client paints names and
        // sidebar groups from them.
        foreach (var role in _roleById.Values.OrderBy(role => role.Position).ThenBy(role => role.Id))
        {
            snapshot.Roles.Add(RoleOf(role));
        }

        foreach (var category in _categories.Values.OrderBy(category => category.Position).ThenBy(category => category.Id))
        {
            snapshot.Categories.Add(CategoryOf(category));
        }

        foreach (var channel in visible)
        {
            snapshot.Channels.Add(ChannelOf(channel));

            // Counters belong to messages, so a voice channel has none; a channel whose visibility
            // changed while the counters were read is simply listed without them.
            if (readStates.TryGetValue(channel.Record.Id, out var read))
            {
                snapshot.ReadStates.Add(ReadStateOf(read));
            }
        }

        foreach (var member in _memberById.Values.OrderBy(member => member.Profile.UserId))
        {
            snapshot.Members.Add(ProfileOf(member));
        }

        // The whole soundpad library, unfiltered: it is server-wide, so there is no reader it is
        // shown a part of.
        foreach (var sound in _soundById.Values.OrderBy(sound => sound.Id))
        {
            snapshot.Sounds.Add(SoundOf(sound));
        }

        foreach (var sticker in _stickerById.Values.OrderBy(sticker => sticker.Id))
        {
            snapshot.Stickers.Add(StickerOf(sticker));
        }

        return new ServerFrame { ServerSnapshot = snapshot };
    }

    private Protocol.Server ServerOf()
        => new()
        {
            Name = _facts.Name,
            Description = _facts.Description,
            IconImageId = _facts.IconImageId ?? 0,
            OwnerId = _facts.OwnerId ?? 0,
            GeneralChannelId = _facts.GeneralChannelId,
        };

    private static Protocol.Role RoleOf(RoleRecord role)
        => new()
        {
            Id = role.Id,
            Name = role.Name,
            Color = (uint)(role.Color ?? 0),
            IconEmoji = role.IconEmoji,
            IconImageId = role.IconImageId ?? 0,
            Position = role.Position,
            Permissions = role.Permissions,
            Hoist = role.Hoist,
            Everyone = role.IsEveryone,
        };

    private static Protocol.Sound SoundOf(SoundRecord sound)
        => new()
        {
            Id = sound.Id,
            Name = sound.Name,
            UploaderId = sound.UploaderId ?? 0,
            DurationMs = (uint)sound.DurationMs,
            Size = sound.Size,
        };

    private static Protocol.Sticker StickerOf(StickerRecord sticker)
        => new()
        {
            Id = sticker.Id,
            Name = sticker.Name,
            UploaderId = sticker.UploaderId ?? 0,
            ContentType = sticker.ContentType,
            Size = sticker.Size,
        };

    private static Protocol.Category CategoryOf(CategoryRecord category)
        => new() { Id = category.Id, Name = category.Name, Position = category.Position };

    private static Protocol.Channel ChannelOf(ChannelState channel)
    {
        var record = channel.Record;
        var frame = new Protocol.Channel
        {
            Id = record.Id,
            Kind = (Protocol.ChannelKind)record.Kind,
            Name = record.Name,
            Topic = record.Topic,
            CategoryId = record.CategoryId ?? 0,
            Position = record.Position,
        };

        if (channel.IsDm)
        {
            // A DM carries its two members and never an override: both of them see it by
            // membership, so there is nothing an override could decide.
            frame.DmMemberIds.Add(record.DmLow ?? 0);
            frame.DmMemberIds.Add(record.DmHigh ?? 0);
            return frame;
        }

        foreach (var entry in channel.Overrides.Values.OrderBy(entry => entry.TargetKind).ThenBy(entry => entry.TargetId))
        {
            frame.Overrides.Add(OverrideOf(entry));
        }

        return frame;
    }

    private static Protocol.Override OverrideOf(OverrideRecord entry)
        => new()
        {
            RoleId = entry.TargetKind == Data.OverrideTarget.Role ? entry.TargetId : 0,
            UserId = entry.TargetKind == Data.OverrideTarget.User ? entry.TargetId : 0,
            Allow = entry.Allow,
            Deny = entry.Deny,
        };

    private Protocol.Profile ProfileOf(MemberState member)
    {
        var profile = member.Profile;
        var frame = new Protocol.Profile
        {
            UserId = profile.UserId,
            Username = profile.Username,
            Nickname = profile.Nickname ?? string.Empty,
            AvatarImageId = profile.AvatarImageId ?? 0,
            BannerImageId = profile.BannerImageId ?? 0,
            Description = profile.Description,
            AccentColor = (uint)(profile.AccentColor ?? 0),
            Online = _online.ContainsKey(profile.UserId),
            ServerMuted = profile.ServerMuted,
            ServerDeafened = profile.ServerDeafened,
        };

        // Highest first, which is the order a client paints a name in; @everyone is held implicitly
        // and never listed.
        foreach (var roleId in member.Roles
            .Where(_roleById.ContainsKey)
            .OrderByDescending(roleId => _roleById[roleId].Position)
            .ThenBy(roleId => roleId))
        {
            frame.RoleIds.Add(roleId);
        }

        return frame;
    }

    private static ReadState ReadStateOf(ReadStateRecord read)
        => new()
        {
            ChannelId = read.ChannelId,
            Unread = read.Unread,
            Mentions = read.Mentions,
            LastMessageId = read.LastMessageId,
        };

    private ServerFrame VoiceStateOf(ChannelState channel)
    {
        var state = new VoiceState { ChannelId = channel.Record.Id };
        foreach (var slot in channel.Voice.Values)
        {
            state.Members.Add(VoiceMemberOf(slot));
        }

        return new ServerFrame { VoiceState = state };
    }

    // The canonical username, never the nickname: that one travels in Profile alone. The flags are
    // the ones last applied to the relay.
    private VoiceMember VoiceMemberOf(VoiceSlot slot)
        => new()
        {
            UserId = slot.Session.UserId,
            Username = _memberById.TryGetValue(slot.Session.UserId, out var member)
                ? member.Profile.Username
                : slot.Connection.Username ?? string.Empty,
            Ssrc = slot.Session.Ssrc,
            Sharing = slot.Sharing,
            ShareAudio = slot.ShareAudio,
            Camera = slot.Camera,
            ServerMuted = slot.Muted,
            ServerDeafened = slot.Deafened,
            Priority = slot.Priority,
            SelfMuted = slot.SelfMuted,
            SelfDeafened = slot.SelfDeafened,
        };

    // Only ever enqueued to the joiner: it carries that session's media key.
    private ServerFrame VoiceReadyOf(ChannelState channel, VoiceSession session)
        => new()
        {
            VoiceReady = new VoiceReady
            {
                ChannelId = channel.Record.Id,
                Host = _relay.AdvertisedHost,
                Port = (uint)_relay.Port,
                Key = ByteString.CopyFrom(session.Key),
                Ssrc = session.Ssrc,
            },
        };

    private static ServerFrame WatchStateOf(ChannelState channel, long sharerUserId)
        => new() { WatchState = new WatchState { ChannelId = channel.Record.Id, UserId = sharerUserId } };

    // The registry's own map is the truth a client is told about: the relay counts the same
    // watchers, but only for diagnostics, and it lags this map by whatever is in flight.
    private static ServerFrame ShareWatchersOf(ChannelState channel, long sharerUserId)
        => new()
        {
            ShareWatchers = new ShareWatchers
            {
                ChannelId = channel.Record.Id,
                Count = (uint)channel.Voice.Values.Count(slot => slot.Watching == sharerUserId),
            },
        };

    // The whole set the viewer watches now, ascending: a client replaces its own set with it
    // rather than patching one camera at a time.
    private static ServerFrame CameraWatchStateOf(ChannelState channel, VoiceSlot viewer)
    {
        var state = new CameraWatchState { ChannelId = channel.Record.Id };
        state.UserIds.AddRange(viewer.WatchingCameras.Order());
        return new ServerFrame { CameraWatchState = state };
    }

    private static ServerFrame CameraWatchersOf(ChannelState channel, long ownerUserId)
        => new()
        {
            CameraWatchers = new CameraWatchers
            {
                ChannelId = channel.Record.Id,
                Count = (uint)channel.Voice.Values.Count(slot => slot.WatchingCameras.Contains(ownerUserId)),
            },
        };

    private ServerFrame MemberUpdatedOf(MemberState member)
        => new() { MemberUpdated = new MemberUpdated { Member = ProfileOf(member) } };

    private static ServerFrame ChannelUpsertedOf(ChannelState channel)
        => new() { ChannelUpserted = new ChannelUpserted { Channel = ChannelOf(channel) } };

    private static ServerFrame ChannelDeletedOf(long channelId)
        => new() { ChannelDeleted = new ChannelDeleted { Id = channelId } };

    private static ServerFrame VoiceMovedOf(long channelId)
        => new() { VoiceMoved = new VoiceMoved { ChannelId = channelId } };

    private static ServerFrame SpeakingOf(long channelId, long userId, bool speaking)
        => new() { Speaking = new Speaking { ChannelId = channelId, UserId = userId, Speaking_ = speaking } };

    private void ReleaseVoice(List<uint> removed)
    {
        foreach (var ssrc in removed)
        {
            _relay.RemoveSession(ssrc);
        }
    }

    // Raised from the relay's threads. No visibility re-check beyond the channel's own audience: a
    // false that lands after the member left says nothing a client cannot already handle, and
    // re-taking _gate here is what keeps this frame ordered against the channel's voice membership.
    private void OnSpeakingChanged(long channelId, long userId, bool speaking)
        => BroadcastToChannel(channelId, SpeakingOf(channelId, userId, speaking));

    // The talk spurts a SetModeration call under _gate ended: the same frame to the same audience
    // the relay's own event would have produced, now that the caller has released the lock.
    private void AnnounceSilenced(List<(long ChannelId, long UserId)> silenced)
    {
        foreach (var (channelId, userId) in silenced)
        {
            BroadcastToChannel(channelId, SpeakingOf(channelId, userId, false));
        }
    }

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

    // The server row as the mirror has it, mutable because UpdateServer and TransferOwnership write
    // these once the row is persisted.
    private sealed class ServerFacts
    {
        public string Name { get; set; } = string.Empty;

        public string Description { get; set; } = string.Empty;

        public long? IconImageId { get; set; }

        // Null until an account exists to own the server; the owner bypasses every check.
        public long? OwnerId { get; set; }

        public long GeneralChannelId { get; set; }
    }

    // One mirrored channel: its persisted facts, its overrides keyed by target, the live voice slots
    // in it, and the permission engine's view of it, cached because every visibility question in
    // every broadcast resolves against it.
    private sealed class ChannelState(ChannelRecord record)
    {
        public ChannelRecord Record { get; set; } = record;

        public Dictionary<(Data.OverrideTarget Kind, long TargetId), OverrideRecord> Overrides { get; } = [];

        // By user id: an account holds at most one slot in a channel.
        public Dictionary<long, VoiceSlot> Voice { get; } = [];

        // What is playing in this channel's voice session, if anything.
        public PlayingSound? Playing { get; set; }

        // Rebuilt by RebuildDefLocked after any override change and whenever general moves.
        public ChannelDef Def { get; set; } = new(record.Id, false, []);

        public bool IsDm => Record.Kind == Data.ChannelKind.Dm;

        public bool IsGeneral => Def.IsGeneral;

        public bool IsDmMember(long userId) => Record.DmLow == userId || Record.DmHigh == userId;
    }

    // One mirrored account: its profile, the roles it holds and, while it is online, the channels it
    // was told it can see.
    private sealed class MemberState(MemberRecord profile)
    {
        public MemberRecord Profile { get; set; } = profile;

        // Never contains the everyone role id: every member holds that one implicitly.
        public HashSet<long> Roles { get; } = [];

        // Maintained for online members only, and cleared when the session ends.
        public HashSet<long> Visible { get; } = [];
    }

    // What is playing in this channel's voice session, if anything. Playback is entirely
    // client-side, so the server is never told a clip ended: a record older than its own
    // duration is finished. PROTOCOL.md § Sounds.
    private sealed record PlayingSound(long SoundId, long UserId, long StartedAtTicks, int DurationMs)
    {
        public bool Finished => Stopwatch.GetElapsedTime(StartedAtTicks).TotalMilliseconds >= DurationMs;
    }

    // One account's live voice session in one channel. The connection is kept alongside the session
    // because the share frames of PROTOCOL.md § Screen share are addressed to one slot at a time.
    private sealed class VoiceSlot(ClientConnection connection, VoiceSession session, bool muted, bool deafened, bool priority)
    {
        public ClientConnection Connection { get; } = connection;

        public VoiceSession Session { get; } = session;

        public bool Sharing { get; set; }

        public bool ShareAudio { get; set; }

        // Whether this session's camera is on, which no share of its own ever touches: the two run
        // side by side.
        public bool Camera { get; set; }

        // The sharer this session is watching, if any. A viewer watches at most one share, and only
        // inside its own channel.
        public long? Watching { get; set; }

        // The cameras this session watches, by user id, at most VoiceOptions.MaxWatchedCameras of
        // them and only inside its own channel.
        public HashSet<long> WatchingCameras { get; } = [];

        // The three moderation flags as last applied to the relay, which is what decides whether a
        // re-resolution has anything to tell it.
        public bool Muted { get; set; } = muted;

        public bool Deafened { get; set; } = deafened;

        public bool Priority { get; set; } = priority;

        // The member's own switches, display state for the other clients only: they are never
        // applied to the relay, so a self-mute stays the member's to undo.
        public bool SelfMuted { get; set; }

        public bool SelfDeafened { get; set; }
    }
}
