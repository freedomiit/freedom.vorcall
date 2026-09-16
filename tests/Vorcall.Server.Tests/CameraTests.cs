using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// The camera signalling only, with no media on the wire. A camera rides a voice session exactly as
// a screen share does and independently of one, so every frame here names the voice channel the
// seed puts beside general — never the general text channel, which has no voice session to ride.
[Collection(ServerCollection.Name)]
public sealed class CameraTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task StartCamera_without_VIDEO_answers_PERMISSION_DENIED_naming_the_bit()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var b1 = party.Client(1);
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);
        await party.SetRoleOverrideAsync(channelId, party.EveryoneId, deny: (ulong)Perm.Video);

        await b1.SendAsync(Frames.JoinVoice(channelId));
        await b1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);

        await b1.SendAsync(Frames.StartCamera(channelId));
        var error = await b1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
        Assert.Equal("VIDEO", error.Detail);
        await party.Client(0).QuietAsync(WsClient.CameraKinds);
    }

    [Fact]
    public async Task StartCamera_outside_voice_answers_NOT_IN_VOICE()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        await party.Client(0).SendAsync(Frames.StartCamera(party.GeneralVoiceId));
        await party.Client(0).ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
        await party.Client(1).QuietAsync(WsClient.CameraKinds);
    }

    // The two kill switches are independent: sharing keeps working with cameras off.
    [Fact]
    public async Task StartCamera_answers_CAMERA_UNAVAILABLE_when_cameras_are_disabled()
    {
        var server = await fixture.CameraDisabledAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);
        var voiceId = a1.Session.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        await a1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);

        await a1.SendAsync(Frames.StartCamera(voiceId));
        await a1.ExpectErrorAsync(ErrorCode.CameraUnavailable, fatal: false);

        await a1.SendAsync(Frames.StartShare(voiceId, audio: false));
        await a1.ExpectSequenceAsync(Kind.ShareStarted, Kind.ShareWatchers);
    }

    [Fact]
    public async Task A_camera_past_the_channels_ceiling_answers_CAMERA_LIMIT()
    {
        var server = await fixture.TightCamerasAsync();
        await using var party = await Party.ConnectAsync(server, "alice", "bob", "carol");
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        for (var i = 0; i < ServerFixture.TightCamerasPerRoom; i++)
        {
            await party.Client(i).SendAsync(Frames.StartCamera(voiceId));
            await StartedEverywhereAsync(party, owner: i, count: 0);
        }

        await party.Client(2).SendAsync(Frames.StartCamera(voiceId));
        await party.Client(2).ExpectErrorAsync(ErrorCode.CameraLimit, fatal: false);

        // An owner repeating itself re-announces the camera it already has, and neither counts
        // against the ceiling nor resets its watchers.
        await party.Client(0).SendAsync(Frames.StartCamera(voiceId));
        await StartedEverywhereAsync(party, owner: 0, count: 0);
    }

    [Fact]
    public async Task CameraStarted_and_CameraStopped_reach_the_whole_channel_and_the_flag_rides_VoiceState()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, carol, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Account(2), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        await a1.SendAsync(Frames.StartCamera(voiceId));
        await StartedEverywhereAsync(party, owner: 0, count: 0);

        // A rejoining member learns the camera from the state it is handed, not from a frame it
        // missed.
        await c1.SendAsync(Frames.LeaveVoice(voiceId));
        await c1.ExpectAsync(Kind.VoiceMemberLeft);
        await c1.SendAsync(Frames.JoinVoice(voiceId));
        var frames = await c1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
        var members = frames[1].VoiceState.Members.ToDictionary(member => member.UserId);
        Assert.True(members[alice.UserId].Camera);
        Assert.False(members[bob.UserId].Camera);
        Assert.False(members[carol.UserId].Camera);

        // The other members hear the rejoin, and the joiner's own camera is off in it.
        var joined = (await a1.ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(carol.UserId, joined.Member.UserId);
        Assert.False(joined.Member.Camera);

        await a1.SendAsync(Frames.StopCamera(voiceId));
        foreach (var client in party.Clients)
        {
            var stopped = (await client.ExpectAsync(Kind.CameraStopped)).CameraStopped;
            Assert.Equal((voiceId, alice.UserId), (stopped.ChannelId, stopped.UserId));
        }

        await a1.SendAsync(Frames.StopCamera(voiceId));
        await a1.ExpectErrorAsync(ErrorCode.NotOnCamera, fatal: false);
    }

    [Fact]
    public async Task WatchCamera_answers_the_viewer_with_its_whole_set_and_moves_the_owners_count()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, b1, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Client(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        foreach (var owner in new[] { 0, 1 })
        {
            await party.Client(owner).SendAsync(Frames.StartCamera(voiceId));
            await StartedEverywhereAsync(party, owner, count: 0);
        }

        await c1.SendAsync(Frames.WatchCamera(voiceId, alice.UserId));
        await ExpectWatchStateAsync(c1, voiceId, alice.UserId);
        Assert.Equal(1u, (await a1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);

        // The set is the whole of it, ascending, not the one camera that changed.
        await c1.SendAsync(Frames.WatchCamera(voiceId, bob.UserId));
        await ExpectWatchStateAsync(c1, voiceId, alice.UserId, bob.UserId);
        Assert.Equal(1u, (await b1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);

        await b1.SendAsync(Frames.WatchCamera(voiceId, alice.UserId));
        await ExpectWatchStateAsync(b1, voiceId, alice.UserId);
        Assert.Equal(2u, (await a1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);

        // Watching one it already watches changes nothing, so the owner is not told its count
        // again.
        await c1.SendAsync(Frames.WatchCamera(voiceId, alice.UserId));
        await ExpectWatchStateAsync(c1, voiceId, alice.UserId, bob.UserId);
        await a1.QuietAsync(WsClient.CameraKinds);

        await c1.SendAsync(Frames.UnwatchCamera(voiceId, alice.UserId));
        await ExpectWatchStateAsync(c1, voiceId, bob.UserId);
        Assert.Equal(1u, (await a1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);
    }

    [Fact]
    public async Task WatchCamera_of_a_camera_that_is_off_or_of_yourself_is_refused()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        await a1.SendAsync(Frames.StartCamera(voiceId));
        await StartedEverywhereAsync(party, owner: 0, count: 0);

        // Bob is in the voice session with no camera on.
        await c1.SendAsync(Frames.WatchCamera(voiceId, bob.UserId));
        await c1.ExpectErrorAsync(ErrorCode.NotOnCamera, fatal: false);

        await a1.SendAsync(Frames.WatchCamera(voiceId, alice.UserId));
        await a1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);
        await a1.QuietAsync(WsClient.CameraKinds);
    }

    [Fact]
    public async Task A_fifth_camera_answers_CAMERA_WATCH_LIMIT()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol", "dave", "erin", "frank");
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        var owners = new[] { 0, 1, 2, 3, 4 };
        foreach (var owner in owners)
        {
            await party.Client(owner).SendAsync(Frames.StartCamera(voiceId));
            await StartedEverywhereAsync(party, owner, count: 0);
        }

        var viewer = party.Client(5);
        var watched = new List<long>();
        foreach (var owner in owners[..4])
        {
            watched.Add(party.Account(owner).UserId);
            await viewer.SendAsync(Frames.WatchCamera(voiceId, party.Account(owner).UserId));
            await ExpectWatchStateAsync(viewer, voiceId, watched.ToArray());
            Assert.Equal(1u, (await party.Client(owner).ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);
        }

        await viewer.SendAsync(Frames.WatchCamera(voiceId, party.Account(4).UserId));
        await viewer.ExpectErrorAsync(ErrorCode.CameraWatchLimit, fatal: false);
        await party.Client(4).QuietAsync(WsClient.CameraKinds);
    }

    [Fact]
    public async Task UnwatchCamera_of_user_0_drops_every_camera_the_viewer_watches()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, b1, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Client(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        foreach (var owner in new[] { 0, 1 })
        {
            await party.Client(owner).SendAsync(Frames.StartCamera(voiceId));
            await StartedEverywhereAsync(party, owner, count: 0);
        }

        foreach (var target in new[] { alice.UserId, bob.UserId })
        {
            await c1.SendAsync(Frames.WatchCamera(voiceId, target));
            await c1.ExpectAsync(Kind.CameraWatchState);
        }

        await a1.ExpectAsync(Kind.CameraWatchers);
        await b1.ExpectAsync(Kind.CameraWatchers);

        await c1.SendAsync(Frames.UnwatchCamera(voiceId));
        await ExpectWatchStateAsync(c1, voiceId);
        Assert.Equal(0u, (await a1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);
        Assert.Equal(0u, (await b1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);

        // Answered even when nothing was being watched.
        await c1.SendAsync(Frames.UnwatchCamera(voiceId));
        await ExpectWatchStateAsync(c1, voiceId);
    }

    [Fact]
    public async Task LeaveVoice_stops_the_camera_and_frees_its_watchers_before_VoiceMemberLeft()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        await a1.SendAsync(Frames.StartCamera(voiceId));
        await StartedEverywhereAsync(party, owner: 0, count: 0);
        await b1.SendAsync(Frames.WatchCamera(voiceId, alice.UserId));
        await b1.ExpectAsync(Kind.CameraWatchState);
        await a1.ExpectAsync(Kind.CameraWatchers);

        await a1.SendAsync(Frames.LeaveVoice(voiceId));

        var watcher = await b1.ExpectSequenceAsync(Kind.CameraWatchState, Kind.CameraStopped, Kind.VoiceMemberLeft);
        Assert.Empty(watcher[0].CameraWatchState.UserIds);
        Assert.Equal(alice.UserId, watcher[1].CameraStopped.UserId);
        Assert.Equal(alice.UserId, watcher[2].VoiceMemberLeft.UserId);

        foreach (var client in new[] { a1, c1 })
        {
            var frames = await client.ExpectSequenceAsync(Kind.CameraStopped, Kind.VoiceMemberLeft);
            Assert.Equal(alice.UserId, frames[0].CameraStopped.UserId);
            Assert.Equal(alice.UserId, frames[1].VoiceMemberLeft.UserId);
        }
    }

    // A DM's two members hold VIDEO whatever the roles say, like the rest of DmGrants.
    [Fact]
    public async Task Both_members_of_a_DM_may_use_a_camera_without_a_role_granting_it()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, bob, b1) = (party.Account(0), party.Client(0), party.Account(1), party.Client(1));
        var dmId = await party.OpenDmAsync(0, 1);

        foreach (var client in new[] { a1, b1 })
        {
            await client.SendAsync(Frames.JoinVoice(dmId));
            await client.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
        }

        await a1.SendAsync(Frames.StartCamera(dmId));
        foreach (var client in new[] { a1, b1 })
        {
            var started = (await client.ExpectAsync(Kind.CameraStarted)).CameraStarted;
            Assert.Equal((dmId, alice.UserId), (started.ChannelId, started.UserId));
        }

        Assert.Equal(0u, (await a1.ExpectAsync(Kind.CameraWatchers)).CameraWatchers.Count);

        await b1.SendAsync(Frames.StartCamera(dmId));
        foreach (var client in new[] { a1, b1 })
        {
            Assert.Equal(bob.UserId, (await client.ExpectAsync(Kind.CameraStarted)).CameraStarted.UserId);
        }
    }

    // The two streams are independent: one member may run both, and stopping one leaves the other
    // alone.
    [Fact]
    public async Task A_screen_share_and_a_camera_run_side_by_side()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        await a1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);

        await a1.SendAsync(Frames.StartShare(voiceId, audio: true));
        await a1.ExpectSequenceAsync(Kind.ShareStarted, Kind.ShareWatchers);
        await a1.SendAsync(Frames.StartCamera(voiceId));
        await a1.ExpectSequenceAsync(Kind.CameraStarted, Kind.CameraWatchers);

        await b1.SendAsync(Frames.JoinVoice(voiceId));
        var frames = await b1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
        var sharer = frames[1].VoiceState.Members.Single(member => member.UserId == alice.UserId);
        Assert.True(sharer.Sharing);
        Assert.True(sharer.ShareAudio);
        Assert.True(sharer.Camera);

        // Ending the share leaves the camera running.
        await a1.SendAsync(Frames.StopShare(voiceId));
        await a1.ExpectAsync(Kind.ShareStopped);

        await c1.SendAsync(Frames.JoinVoice(voiceId));
        var later = await c1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
        var stillOnCamera = later[1].VoiceState.Members.Single(member => member.UserId == alice.UserId);
        Assert.False(stillOnCamera.Sharing);
        Assert.True(stillOnCamera.Camera);
    }

    // Every member joins the seeded voice channel in party order, each with an ssrc of its own. The
    // joiner hears VoiceReady then VoiceState; the members already in are told by VoiceMemberJoined,
    // which the next joiner's own reads leave behind.
    private static async Task JoinVoiceAsync(Party party)
    {
        var voiceId = party.GeneralVoiceId;
        VoiceState? state = null;
        for (var i = 0; i < party.Clients.Count; i++)
        {
            var (account, client) = (party.Account(i), party.Client(i));
            await client.SendAsync(Frames.JoinVoice(voiceId));
            var frames = await client.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
            Assert.NotEqual(0u, frames[0].VoiceReady.Ssrc);
            state = frames[1].VoiceState;
            Assert.Contains(state.Members, member => member.UserId == account.UserId);
        }

        Assert.All(state!.Members, member => Assert.False(member.Camera));

        // The joins the earlier members were told about, consumed so that later reads see only the
        // camera frames they are about.
        for (var i = 0; i < party.Clients.Count; i++)
        {
            for (var later = i + 1; later < party.Clients.Count; later++)
            {
                var joined = (await party.Client(i).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
                Assert.Equal(party.Account(later).UserId, joined.Member.UserId);
            }
        }
    }

    // CameraStarted reaches every viewer of the channel, the owner included; the owner then hears
    // its watcher count.
    private static async Task StartedEverywhereAsync(Party party, int owner, uint count)
    {
        var voiceId = party.GeneralVoiceId;
        for (var i = 0; i < party.Clients.Count; i++)
        {
            var client = party.Client(i);
            CameraStarted started;
            if (i == owner)
            {
                var frames = await client.ExpectSequenceAsync(Kind.CameraStarted, Kind.CameraWatchers);
                started = frames[0].CameraStarted;
                Assert.Equal(voiceId, frames[1].CameraWatchers.ChannelId);
                Assert.Equal(count, frames[1].CameraWatchers.Count);
            }
            else
            {
                started = (await client.ExpectAsync(Kind.CameraStarted)).CameraStarted;
            }

            Assert.Equal(voiceId, started.ChannelId);
            Assert.Equal(party.Account(owner).UserId, started.UserId);
        }
    }

    // The viewer's whole watched set, which the server always answers with in full and ascending.
    private static async Task ExpectWatchStateAsync(WsClient client, long channelId, params long[] userIds)
    {
        var state = (await client.ExpectAsync(Kind.CameraWatchState)).CameraWatchState;
        Assert.Equal(channelId, state.ChannelId);
        Assert.Equal(userIds.Order().ToArray(), state.UserIds.ToArray());
    }
}
