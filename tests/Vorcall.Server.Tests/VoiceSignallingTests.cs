using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// PROTOCOL.md § Voice: the signalling only, with no media on the wire. Voice lives on a voice
// channel, never on a text one, so the subject throughout is the voice channel the seed puts
// beside general, or a voice channel the owner creates for the test.
[Collection(ServerCollection.Name)]
public sealed class VoiceSignallingTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task JoinVoice_answers_VoiceReady_with_a_unique_ssrc_and_the_VoiceState()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (bob, b1) = (party.Account(1), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        // Never asserted or printed whole: VoiceReady carries the media key.
        await a1.SendAsync(Frames.JoinVoice(voiceId));
        var aliceReady = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        Assert.Equal(voiceId, aliceReady.ChannelId);
        Assert.NotEqual(0u, aliceReady.Ssrc);
        Assert.Equal((uint)Server.VoicePort, aliceReady.Port);
        Assert.Equal("127.0.0.1", aliceReady.Host);
        Assert.Equal(32, aliceReady.Key.Length);
        var aliceState = (await a1.ExpectAsync(Kind.VoiceState)).VoiceState;
        Assert.Equal(voiceId, aliceState.ChannelId);
        var self = Assert.Single(aliceState.Members);
        Assert.Equal(alice.UserId, self.UserId);
        Assert.Equal(aliceReady.Ssrc, self.Ssrc);

        // @everyone grants SPEAK and not PRIORITY_SPEAKER, so an ordinary joiner is neither muted
        // nor a priority speaker.
        Assert.False(self.ServerMuted);
        Assert.False(self.ServerDeafened);
        Assert.False(self.Priority);

        await b1.SendAsync(Frames.JoinVoice(voiceId));
        var bobReady = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        Assert.NotEqual(0u, bobReady.Ssrc);
        Assert.NotEqual(aliceReady.Ssrc, bobReady.Ssrc);
        var bobState = (await b1.ExpectAsync(Kind.VoiceState)).VoiceState;
        Assert.Equal(
            new[] { alice.UserId, bob.UserId }.Order().ToArray(),
            bobState.Members.Select(member => member.UserId).Order().ToArray());
        Assert.All(bobState.Members, member => Assert.False(member.Sharing));
    }

    [Fact]
    public async Task Voice_joins_and_leaves_are_announced_to_every_viewer_of_the_channel()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        var ready = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        await a1.ExpectAsync(Kind.VoiceState);

        // Bob is not in voice and still hears it: the audience is every member who may view the
        // channel. The joiner itself is not told, its own VoiceState having said as much.
        var joined = (await b1.ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(voiceId, joined.ChannelId);
        Assert.Equal(alice.UserId, joined.Member.UserId);
        Assert.Equal(alice.Username, joined.Member.Username);
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        await a1.QuietAsync(WsClient.VoiceKinds);

        // A leave reaches the leaver too.
        await a1.SendAsync(Frames.LeaveVoice(voiceId));
        foreach (var client in new[] { a1, b1 })
        {
            var left = (await client.ExpectAsync(Kind.VoiceMemberLeft)).VoiceMemberLeft;
            Assert.Equal(voiceId, left.ChannelId);
            Assert.Equal(alice.UserId, left.UserId);
        }
    }

    [Fact]
    public async Task JoinVoice_on_a_text_channel_answers_INVALID_ARGUMENT()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        // general is a text channel: it holds messages and no voice session at all.
        Assert.Equal(ChannelKind.Text, a1.Session.General.Kind);
        await a1.SendAsync(Frames.JoinVoice(a1.Session.GeneralId));
        await a1.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false);
        await a1.QuietAsync(WsClient.VoiceKinds);
    }

    [Fact]
    public async Task LeaveVoice_while_not_in_the_channel_answers_NOT_IN_VOICE()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.LeaveVoice(a1.Session.GeneralVoiceId));
        await a1.ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
    }

    // PROTOCOL.md § Voice: a joiner without SPEAK is muted rather than refused, and the flag is on
    // its session from the first packet.
    [Fact]
    public async Task A_member_without_SPEAK_joins_server_muted()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);
        await party.SetRoleOverrideAsync(channelId, party.EveryoneId, deny: (ulong)Perm.Speak);

        await b1.SendAsync(Frames.JoinVoice(channelId));
        var ready = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        var member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.Equal(bob.UserId, member.UserId);
        Assert.True(member.ServerMuted);
        Assert.False(member.Priority);

        // The channel's other viewers are told the same flag.
        var joined = (await party.Client(0).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(channelId, joined.ChannelId);
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        Assert.True(joined.Member.ServerMuted);
    }

    // PRIORITY_SPEAKER is resolved in the channel, so it is a role the member holds rather than a
    // flag a client sets.
    [Fact]
    public async Task PRIORITY_SPEAKER_in_the_channel_marks_the_voice_member()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);

        // Granted before the join: the flags are read when the session is created.
        await party.GrantRoleAsync(1, (ulong)Perm.PrioritySpeaker);

        await b1.SendAsync(Frames.JoinVoice(channelId));
        var ready = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        var member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.Equal(bob.UserId, member.UserId);
        Assert.True(member.Priority);
        Assert.False(member.ServerMuted);

        var joined = (await party.Client(0).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        Assert.True(joined.Member.Priority);
    }

    // PROTOCOL.md § Voice: the joiner's own switches ride JoinVoice, so a client that rejoins
    // muted is never drawn unmuted while it says so.
    [Fact]
    public async Task A_joiners_own_switches_are_on_the_wire_from_the_first_frame()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId, selfMuted: true, selfDeafened: true));
        await a1.ExpectAsync(Kind.VoiceReady);
        var self = Assert.Single((await a1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.True(self.SelfMuted);
        Assert.True(self.SelfDeafened);

        // Display only: a member's own switches are not the moderators'.
        Assert.False(self.ServerMuted);
        Assert.False(self.ServerDeafened);

        // The channel's other viewers are told the same in the join frame.
        var joined = (await b1.ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(alice.UserId, joined.Member.UserId);
        Assert.True(joined.Member.SelfMuted);
        Assert.True(joined.Member.SelfDeafened);
        Assert.False(joined.Member.ServerMuted);
        Assert.False(joined.Member.ServerDeafened);
    }

    [Fact]
    public async Task VoiceSelfState_re_broadcasts_the_VoiceState_to_every_viewer_of_the_channel()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        await a1.ExpectAsync(Kind.VoiceReady);
        var initial = Assert.Single((await a1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.False(initial.SelfMuted);
        Assert.False(initial.SelfDeafened);
        await b1.ExpectAsync(Kind.VoiceMemberJoined);

        await a1.SendAsync(Frames.VoiceSelfState(voiceId, muted: true, deafened: false));

        // Bob is in no voice session at all and still learns it: the audience is every member who
        // may view the channel.
        foreach (var client in new[] { a1, b1 })
        {
            var state = (await client.ExpectAsync(Kind.VoiceState)).VoiceState;
            Assert.Equal(voiceId, state.ChannelId);
            var member = Assert.Single(state.Members);
            Assert.Equal(alice.UserId, member.UserId);
            Assert.True(member.SelfMuted);
            Assert.False(member.SelfDeafened);
            Assert.False(member.ServerMuted);
            Assert.False(member.ServerDeafened);
        }
    }

    [Fact]
    public async Task VoiceSelfState_that_changes_nothing_broadcasts_nothing()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (a1, b1) = (party.Client(0), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        await a1.ExpectAsync(Kind.VoiceReady);
        await a1.ExpectAsync(Kind.VoiceState);
        await b1.ExpectAsync(Kind.VoiceMemberJoined);

        await a1.SendAsync(Frames.VoiceSelfState(voiceId, muted: true, deafened: true));
        foreach (var client in new[] { a1, b1 })
        {
            var member = Assert.Single((await client.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
            Assert.True(member.SelfMuted);
            Assert.True(member.SelfDeafened);
        }

        // The same state again: a client re-asserting what the server already holds must not make
        // it fan a VoiceState out to the channel for nothing. Frames of one connection are handled
        // in order, so a VoiceState broadcast for it would sit ahead of the pong — and the strict
        // read skips nothing, unlike ExpectAsync.
        await a1.SendAsync(Frames.VoiceSelfState(voiceId, muted: true, deafened: true));
        await a1.SendAsync(Frames.Ping(7));
        var next = await a1.ReceiveStrictAsync();
        Assert.Equal(Kind.Pong, next.PayloadCase);
        Assert.Equal(7L, next.Pong.SentAtUnixMs);
        await b1.QuietAsync(WsClient.VoiceKinds);
    }

    [Fact]
    public async Task VoiceSelfState_while_not_in_the_voice_session_answers_NOT_IN_VOICE()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.VoiceSelfState(a1.Session.GeneralVoiceId, muted: true, deafened: false));
        await a1.ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
        await a1.QuietAsync(WsClient.VoiceKinds);
    }

    // PROTOCOL.md § Voice: VoiceSelfState answers UNKNOWN_CHANNEL for an unknown or invisible
    // channel, and 0 is no channel id at all — it is refused before the voice session is looked up,
    // so it is that error and not NOT_IN_VOICE.
    [Fact]
    public async Task VoiceSelfState_for_channel_0_answers_UNKNOWN_CHANNEL()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.VoiceSelfState(0, muted: true, deafened: false));
        await a1.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
        await a1.QuietAsync(WsClient.VoiceKinds);
    }

    // The two pairs never move each other: a member's own switches are display state the relay is
    // never told about, and the moderation flags are not the member's to set.
    [Fact]
    public async Task Self_flags_and_the_moderation_flags_are_independent()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);
        await party.SetRoleOverrideAsync(channelId, party.EveryoneId, deny: (ulong)Perm.Speak);

        // Without SPEAK the session is server-muted, which says nothing about the switches the
        // member itself arrived with.
        await b1.SendAsync(Frames.JoinVoice(channelId));
        await b1.ExpectAsync(Kind.VoiceReady);
        var member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.True(member.ServerMuted);
        Assert.False(member.SelfMuted);
        Assert.False(member.SelfDeafened);

        // And the other way round: a self-mute leaves the moderation flags exactly as they were.
        await b1.SendAsync(Frames.VoiceSelfState(channelId, muted: true, deafened: true));
        member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.Equal(bob.UserId, member.UserId);
        Assert.True(member.SelfMuted);
        Assert.True(member.SelfDeafened);
        Assert.True(member.ServerMuted);
        Assert.False(member.ServerDeafened);

        // The owner's socket still holds the join and that self-state broadcast ahead of what the
        // moderation is about to send, and ExpectAsync would hand the older VoiceState back; they
        // are read off here so the next one is unambiguously the moderated state.
        var o1 = party.Client(0);
        await o1.ExpectAsync(Kind.VoiceMemberJoined);
        Assert.True(Assert.Single((await o1.ExpectAsync(Kind.VoiceState)).VoiceState.Members).SelfMuted);

        // PROTOCOL.md § Moderation: a moderator's flags rewrite the session's moderation pair and
        // nothing else, so the switches the member set for itself survive the frame. Deafen rather
        // than mute, the mute already being held on by the missing SPEAK.
        await o1.SendAsync(Frames.VoiceModerate(bob.UserId, channelId, deafened: true));
        foreach (var client in new[] { b1, o1 })
        {
            var moderated = Assert.Single((await client.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
            Assert.Equal(bob.UserId, moderated.UserId);
            Assert.True(moderated.ServerDeafened);
            Assert.True(moderated.ServerMuted);
            Assert.True(moderated.SelfMuted);
            Assert.True(moderated.SelfDeafened);
        }

        // The moderation pair is profile state, so it is also restated to every online member.
        foreach (var client in new[] { b1, o1 })
        {
            var profile = await client.ExpectMemberUpdatedAsync(bob);
            Assert.True(profile.ServerDeafened);
            Assert.False(profile.ServerMuted);
        }
    }

    [Fact]
    public async Task JoinVoice_answers_VOICE_UNAVAILABLE_when_the_relay_is_disabled()
    {
        var server = await fixture.VoiceDisabledAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);
        await a1.SendAsync(Frames.JoinVoice(a1.Session.GeneralVoiceId));
        await a1.ExpectErrorAsync(ErrorCode.VoiceUnavailable, fatal: false);
    }
}
