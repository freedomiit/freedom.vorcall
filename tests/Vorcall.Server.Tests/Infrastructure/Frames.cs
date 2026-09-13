using Vorcall.Server.Protocol;

namespace Vorcall.Server.Tests.Infrastructure;

// One builder per ClientFrame payload, each named after the payload it carries. Channels are
// numeric ids the server assigned, so every channel argument is a long: there is no slug to
// predict and no membership to join.
internal static class Frames
{
    public static ClientFrame Hello(uint protocolVersion = 1, string clientVersion = "", string clientPlatform = "")
        => new() { Hello = new Hello { ProtocolVersion = protocolVersion, ClientVersion = clientVersion, ClientPlatform = clientPlatform } };

    public static ClientFrame Send(string text, long channelId, long replyToId = 0, params long[] attachmentIds)
    {
        var send = new SendMessage { Text = text, ChannelId = channelId, ReplyToId = replyToId };
        send.AttachmentIds.AddRange(attachmentIds);
        return new ClientFrame { Send = send };
    }

    // The same, carrying streamed files rather than uploads; text may be empty for either.
    public static ClientFrame SendStreamed(string text, long channelId, params long[] streamedFileIds)
    {
        var send = new SendMessage { Text = text, ChannelId = channelId };
        send.StreamedFileIds.AddRange(streamedFileIds);
        return new ClientFrame { Send = send };
    }

    public static ClientFrame Ping(long sentAtUnixMs) => new() { Ping = new Ping { SentAtUnixMs = sentAtUnixMs } };

    public static ClientFrame OpenDm(long userId) => new() { OpenDm = new OpenDm { UserId = userId } };

    public static ClientFrame MarkRead(long channelId, long messageId)
        => new() { MarkRead = new MarkRead { ChannelId = channelId, MessageId = messageId } };

    public static ClientFrame Edit(long messageId, string text) => new() { EditMessage = new EditMessage { Id = messageId, Text = text } };

    public static ClientFrame Delete(long messageId) => new() { DeleteMessage = new DeleteMessage { Id = messageId } };

    public static ClientFrame React(long messageId, string emoji, bool remove = false)
        => new() { React = new React { MessageId = messageId, Emoji = emoji, Remove = remove } };

    public static ClientFrame JoinVoice(long channelId, bool selfMuted = false, bool selfDeafened = false)
        => new() { JoinVoice = new JoinVoice { ChannelId = channelId, SelfMuted = selfMuted, SelfDeafened = selfDeafened } };

    public static ClientFrame LeaveVoice(long channelId) => new() { LeaveVoice = new LeaveVoice { ChannelId = channelId } };

    public static ClientFrame VoiceSelfState(long channelId, bool muted, bool deafened)
        => new() { VoiceSelfState = new VoiceSelfState { ChannelId = channelId, Muted = muted, Deafened = deafened } };

    public static ClientFrame StartShare(long channelId, bool audio = false)
        => new() { StartShare = new StartShare { ChannelId = channelId, Audio = audio } };

    public static ClientFrame StopShare(long channelId) => new() { StopShare = new StopShare { ChannelId = channelId } };

    public static ClientFrame WatchShare(long channelId, long userId)
        => new() { WatchShare = new WatchShare { ChannelId = channelId, UserId = userId } };

    public static ClientFrame UnwatchShare(long channelId) => new() { UnwatchShare = new UnwatchShare { ChannelId = channelId } };

    public static ClientFrame CreateChannel(ChannelKind kind, string name, string topic = "", long categoryId = 0)
        => new() { CreateChannel = new CreateChannel { Kind = kind, Name = name, Topic = topic, CategoryId = categoryId } };

    public static ClientFrame UpdateChannel(long id, string name, string topic = "")
        => new() { UpdateChannel = new UpdateChannel { Id = id, Name = name, Topic = topic } };

    public static ClientFrame DeleteChannel(long id) => new() { DeleteChannel = new DeleteChannel { Id = id } };

    public static ClientFrame CreateCategory(string name) => new() { CreateCategory = new CreateCategory { Name = name } };

    public static ClientFrame UpdateCategory(long id, string name)
        => new() { UpdateCategory = new UpdateCategory { Id = id, Name = name } };

    public static ClientFrame DeleteCategory(long id) => new() { DeleteCategory = new DeleteCategory { Id = id } };

    // Takes the full list of non-DM channels, which is what the server expects.
    public static ClientFrame ReorderChannels(params ChannelPosition[] positions)
    {
        var reorder = new ReorderChannels();
        reorder.Positions.AddRange(positions);
        return new ClientFrame { ReorderChannels = reorder };
    }

    public static ChannelPosition At(long channelId, long categoryId, int position)
        => new() { Id = channelId, CategoryId = categoryId, Position = position };

    // The full list; position = index.
    public static ClientFrame ReorderCategories(params long[] ids)
    {
        var reorder = new ReorderCategories();
        reorder.Ids.AddRange(ids);
        return new ClientFrame { ReorderCategories = reorder };
    }

    // allow == deny == 0 deletes the override.
    public static ClientFrame SetOverride(long channelId, Override over)
        => new() { SetOverride = new SetOverride { ChannelId = channelId, Override = over } };

    public static Override RoleOverride(long roleId, ulong allow = 0, ulong deny = 0)
        => new() { RoleId = roleId, Allow = allow, Deny = deny };

    public static Override MemberOverride(long userId, ulong allow = 0, ulong deny = 0)
        => new() { UserId = userId, Allow = allow, Deny = deny };

    public static ClientFrame CreateRole(
        string name,
        ulong permissions = 0,
        uint color = 0,
        string iconEmoji = "",
        long iconImageId = 0,
        bool hoist = false)
        => new()
        {
            CreateRole = new CreateRole
            {
                Name = name,
                Permissions = permissions,
                Color = color,
                IconEmoji = iconEmoji,
                IconImageId = iconImageId,
                Hoist = hoist,
            },
        };

    // Replaces the role wholesale; its position is ignored, reordering being its own frame.
    public static ClientFrame UpdateRole(Role role) => new() { UpdateRole = new UpdateRole { Role = role } };

    public static ClientFrame DeleteRole(long id) => new() { DeleteRole = new DeleteRole { Id = id } };

    // The full list, @everyone excluded, bottom first: position = index + 1.
    public static ClientFrame ReorderRoles(params long[] idsBottomFirst)
    {
        var reorder = new ReorderRoles();
        reorder.Ids.AddRange(idsBottomFirst);
        return new ClientFrame { ReorderRoles = reorder };
    }

    public static ClientFrame SetMemberRoles(long userId, params long[] roleIds)
    {
        var set = new SetMemberRoles { UserId = userId };
        set.RoleIds.AddRange(roleIds);
        return new ClientFrame { SetMemberRoles = set };
    }

    // user_id 0 means self; an empty nickname clears it.
    public static ClientFrame SetNickname(long userId, string nickname)
        => new() { SetNickname = new SetNickname { UserId = userId, Nickname = nickname } };

    public static ClientFrame KickMember(long userId) => new() { KickMember = new KickMember { UserId = userId } };

    public static ClientFrame BanMember(long userId, string reason = "")
        => new() { BanMember = new BanMember { UserId = userId, Reason = reason } };

    public static ClientFrame UnbanMember(long userId) => new() { UnbanMember = new UnbanMember { UserId = userId } };

    public static ClientFrame UpdateServer(string name, string description = "", long iconImageId = 0)
        => new() { UpdateServer = new UpdateServer { Name = name, Description = description, IconImageId = iconImageId } };

    public static ClientFrame TransferOwnership(long userId)
        => new() { TransferOwnership = new TransferOwnership { UserId = userId } };

    // Each flag of the frame applies only with its set_* companion, so a null argument here is a
    // flag left alone; moveTo 0 disconnects the target.
    public static ClientFrame VoiceModerate(
        long userId,
        long channelId,
        bool? muted = null,
        bool? deafened = null,
        long? moveTo = null)
        => new()
        {
            VoiceModerate = new VoiceModerate
            {
                UserId = userId,
                ChannelId = channelId,
                SetMuted = muted is not null,
                Muted = muted ?? false,
                SetDeafened = deafened is not null,
                Deafened = deafened ?? false,
                Move = moveTo is not null,
                MoveTo = moveTo ?? 0,
            },
        };

    // The two image fields are sentinel-coded on the wire: 0 keeps the current image, -1 clears it.
    public static ClientFrame UpdateProfile(
        string description = "",
        uint accentColor = 0,
        long avatarImageId = 0,
        long bannerImageId = 0)
        => new()
        {
            UpdateProfile = new UpdateProfile
            {
                Description = description,
                AccentColor = accentColor,
                AvatarImageId = avatarImageId,
                BannerImageId = bannerImageId,
            },
        };

    public static ClientFrame PlaySound(long channelId, long soundId)
        => new() { PlaySound = new PlaySound { ChannelId = channelId, SoundId = soundId } };

    public static ClientFrame StopSound(long channelId) => new() { StopSound = new StopSound { ChannelId = channelId } };

    public static ClientFrame UpdateSound(long soundId, string name)
        => new() { UpdateSound = new UpdateSound { SoundId = soundId, Name = name } };

    public static ClientFrame DeleteSound(long soundId) => new() { DeleteSound = new DeleteSound { SoundId = soundId } };

    public static long NowMs() => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
}
