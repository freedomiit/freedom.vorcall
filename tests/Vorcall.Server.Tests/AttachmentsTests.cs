using System.Net;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class AttachmentsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Upload_accepts_any_type_and_enforces_the_content_type_and_the_channel()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "carol");
        var (owner, carol) = (party.Account(0), party.Account(1));
        var general = party.GeneralId;
        var pixel = Png.Pixel();

        var stored = await Uploads.UploadAsync(Server, owner.Access, general, pixel, fileName: "pixel.png");
        Assert.Equal(HttpStatusCode.Created, stored.Status);
        var attachment = stored.As(Attachment.Parser);
        Assert.True(attachment.Id > 0);
        Assert.Equal("pixel.png", attachment.FileName);
        Assert.Equal("image/png", attachment.ContentType);
        Assert.Equal((long)pixel.Length, attachment.Size);

        var bmp = await Uploads.UploadAsync(Server, owner.Access, general, pixel, contentType: "image/bmp");
        Assert.Equal(HttpStatusCode.Created, bmp.Status);
        Assert.Equal("image/bmp", bmp.As(Attachment.Parser).ContentType);

        var notAnImageBody = "MZ not an image at all"u8.ToArray();
        var notAnImage = await Uploads.UploadAsync(Server, owner.Access, general, notAnImageBody);
        Assert.Equal(HttpStatusCode.Created, notAnImage.Status);
        Assert.Equal((long)notAnImageBody.Length, notAnImage.As(Attachment.Parser).Size);

        // Attachments accept any media type, but it still has to be one: a header whose type and
        // subtype are each well-formed tokens but together run past the 128-character cap is
        // refused as malformed rather than silently truncated.
        var overlongType = new string('a', 70) + "/" + new string('b', 70);
        var malformedType = await Uploads.UploadAsync(Server, owner.Access, general, pixel, contentType: overlongType);
        Assert.Equal(HttpStatusCode.BadRequest, malformedType.Status);
        Assert.Equal("malformed content type", malformedType.Detail);

        // The channel is a numeric id and has to hold messages for anything to link the upload to.
        foreach (var channel in new[] { "bad!", "0", History.Id(party.GeneralVoiceId) })
        {
            var refused = await Uploads.UploadAsync(Server, owner.Access, channel, pixel);
            Assert.Equal(HttpStatusCode.BadRequest, refused.Status);
            Assert.Equal("channel", refused.Detail);
        }

        // Sight of the channel, then the right to attach in it: two different refusals, each naming
        // the bit that is missing.
        await party.SetMemberOverrideAsync(general, 1, deny: (ulong)Perm.AttachFiles);
        var cannotAttach = await Uploads.UploadAsync(Server, carol.Access, general, pixel);
        Assert.Equal(HttpStatusCode.Forbidden, cannotAttach.Status);
        Assert.Equal(PermNames.Name(Perm.AttachFiles), cannotAttach.Detail);

        // Cleared again: general is shared by the whole collection.
        await party.SetMemberOverrideAsync(general, 1);

        var (_, hiddenId) = await party.CreateChannelAsync();
        await party.Client(0).SendAsync(
            Frames.SetOverride(hiddenId, Frames.MemberOverride(carol.UserId, deny: (ulong)Perm.ViewChannel)));
        await party.Client(1).ExpectChannelDeletedAsync(hiddenId);
        await party.Client(0).ExpectChannelUpsertedAsync(hiddenId);

        var outsider = await Uploads.UploadAsync(Server, carol.Access, hiddenId, pixel);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), outsider.Detail);
    }

    [Fact]
    public async Task An_unlinked_upload_belongs_to_its_uploader_alone()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, bob) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, party.GeneralId, pixel);

        var stranger = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, stranger.Status);

        var owner = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, owner.Status);
        Assert.Equal(pixel, owner.Body);
    }

    [Fact]
    public async Task A_linked_upload_is_readable_by_the_channels_viewers_with_ETag_and_Range()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob) = (party.Account(0), party.Client(0), party.Account(1));
        var general = party.GeneralId;
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, general, pixel);

        var caption = $"look at this {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, general, 0, attachment.Id));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.Message))
        {
            var message = frame.Message;
            Assert.Equal(caption, message.Text);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
            Assert.Equal("pixel.png", message.Attachments[0].FileName);
            Assert.Equal((long)pixel.Length, message.Attachments[0].Size);
        }

        var whole = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, whole.Status);
        Assert.Equal(pixel, whole.Body);
        Assert.Equal($"\"{attachment.Id}-{pixel.Length}\"", whole.Header("ETag"));

        var head = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id, range: "bytes=0-3");
        Assert.Equal(HttpStatusCode.PartialContent, head.Status);
        Assert.Equal(pixel[..4], head.Body);
    }

    [Fact]
    public async Task A_DM_image_is_invisible_outside_the_DM()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var (bob, carol, c1) = (party.Account(1), party.Account(2), party.Client(2));
        var dmId = await party.OpenDmAsync(0, 1);

        var attachment = await UploadPixelAsync(alice, dmId, Png.Pixel(), "private.png");
        await a1.SendAsync(Frames.Send(string.Empty, dmId, 0, attachment.Id));
        foreach (var client in new[] { a1, b1 })
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(string.Empty, message.Text);
            Assert.Equal(dmId, message.ChannelId);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
        }

        var outsider = await Uploads.DownloadAsync(Server, carol.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        var member = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, member.Status);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task Reuse_or_too_many_attachments_answer_INVALID_ATTACHMENT_and_deleting_the_message_removes_the_file()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;
        var attachment = await UploadPixelAsync(alice, general, Png.Pixel());

        var caption = $"linked {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, general, 0, attachment.Id));
        var imageMessageId = await party.ExpectMessageEverywhereAsync(caption, general);

        await a1.SendAsync(Frames.Send("again", general, 0, attachment.Id));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        var five = Enumerable.Range(1, 5).Select(offset => attachment.Id + offset).ToArray();
        await a1.SendAsync(Frames.Send("five", general, 0, five));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        await a1.SendAsync(Frames.Delete(imageMessageId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(imageMessageId, frame.MessageDeleted.Id);
        }

        var gone = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
        var store = Server.Services.GetRequiredService<AttachmentStore>();
        Assert.False(File.Exists(store.PathFor(attachment.Id, attachment.ContentType)));
    }

    [Fact]
    public async Task Every_media_type_is_stored_lower_cased_and_a_header_that_is_not_one_is_refused()
    {
        await using var party = await Party.ConnectAsync(Server, "olga");
        var olga = party.Account(0);
        var general = party.GeneralId;

        // Nothing on this path interprets the bytes, so the type a client declares is metadata:
        // a document, a text file and an unstated kind of bytes are all just files.
        var pdf = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Pdf(), "application/pdf", "notes.pdf");
        Assert.Equal(HttpStatusCode.Created, pdf.Status);
        Assert.Equal("application/pdf", pdf.As(Attachment.Parser).ContentType);

        var text = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Text("one line\n"), "text/plain", "a.txt");
        Assert.Equal(HttpStatusCode.Created, text.Status);
        Assert.Equal("text/plain", text.As(Attachment.Parser).ContentType);

        var octets = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Of(64), StreamApi.Octets);
        Assert.Equal(HttpStatusCode.Created, octets.Status);
        Assert.Equal(StreamApi.Octets, octets.As(Attachment.Parser).ContentType);

        // A body that declares no type at all is bytes of an unstated kind, which is what
        // application/octet-stream means; refusing it would gain nothing.
        var untyped = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Of(64), contentType: null);
        Assert.Equal(HttpStatusCode.Created, untyped.Status);
        Assert.Equal(StreamApi.Octets, untyped.As(Attachment.Parser).ContentType);

        // Media types are case-insensitive and the row keeps them lower case.
        var shouted = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Pdf(), "APPLICATION/PDF");
        Assert.Equal(HttpStatusCode.Created, shouted.Status);
        Assert.Equal("application/pdf", shouted.As(Attachment.Parser).ContentType);

        // Any type, as long as it is a type. A second slash is not a media type at all, and 128
        // characters is what the column holds, so one character past that is refused rather than
        // stored truncated.
        var twoSlashes = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Of(8), contentType: null, rawContentType: "a/b/c");
        Assert.Equal(HttpStatusCode.BadRequest, twoSlashes.Status);
        Assert.Equal("malformed content type", twoSlashes.Detail);

        var atTheCap = new string('a', 63) + "/" + new string('b', 64);
        Assert.Equal(128, atTheCap.Length);
        var accepted = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Of(8), atTheCap);
        Assert.Equal(HttpStatusCode.Created, accepted.Status);
        Assert.Equal(atTheCap, accepted.As(Attachment.Parser).ContentType);

        var overTheCap = new string('a', 64) + "/" + new string('b', 64);
        Assert.Equal(129, overTheCap.Length);
        var refused = await Uploads.UploadAsync(Server, olga.Access, general, Blob.Of(8), overTheCap);
        Assert.Equal(HttpStatusCode.BadRequest, refused.Status);
        Assert.Equal("malformed content type", refused.Detail);
    }

    [Fact]
    public async Task A_file_of_any_length_is_stored_and_read_back_whole()
    {
        await using var party = await Party.ConnectAsync(Server, "petra");
        var petra = party.Account(0);
        var general = party.GeneralId;

        // Nothing sniffs an attachment any more, so a file with no room for a magic number in it
        // is a file like any other.
        var tiny = Blob.Text("abc");
        var short3 = await Uploads.UploadAsync(Server, petra.Access, general, tiny, "text/plain", "abc.txt");
        Assert.Equal(HttpStatusCode.Created, short3.Status);
        Assert.Equal(3L, short3.As(Attachment.Parser).Size);
        var back = await Uploads.DownloadAsync(Server, petra.Access, short3.As(Attachment.Parser).Id);
        Assert.Equal(HttpStatusCode.OK, back.Status);
        Assert.Equal(tiny, back.Body);

        var empty = await Uploads.UploadAsync(Server, petra.Access, general, [], "text/plain", "empty.txt");
        Assert.Equal(HttpStatusCode.Created, empty.Status);
        var emptyRow = empty.As(Attachment.Parser);
        Assert.Equal(0L, emptyRow.Size);
        var nothing = await Uploads.DownloadAsync(Server, petra.Access, emptyRow.Id);
        Assert.Equal(HttpStatusCode.OK, nothing.Status);
        Assert.Empty(nothing.Body);
    }

    [Fact]
    public async Task An_upload_past_two_gibibytes_is_refused_whichever_end_of_the_body_says_so()
    {
        await using var party = await Party.ConnectAsync(Server, "quinn");
        var quinn = party.Account(0);
        var general = party.GeneralId;
        var body = Blob.Of(64);

        // The declared length alone, before a byte of the body is read.
        var declared = await Uploads.UploadAsync(
            Server,
            quinn.Access,
            general,
            body,
            StreamApi.Octets,
            declaredLength: AttachmentsOptions.MaxFileBytes + 1);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, declared.Status);
        Assert.Equal("files must be 2 GiB or smaller", declared.Detail);

        // And again from inside the store, for a body that runs past what its header promised:
        // one refusal, because from outside a lying length and an oversized file are the same
        // mistake.
        var overrun = await Uploads.UploadAsync(Server, quinn.Access, general, body, StreamApi.Octets, declaredLength: 8);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, overrun.Status);
        Assert.Equal("files must be 2 GiB or smaller", overrun.Detail);
    }

    [Fact]
    public async Task A_download_may_be_neither_sniffed_nor_rendered_on_this_origin()
    {
        await using var party = await Party.ConnectAsync(Server, "rosa");
        var rosa = party.Account(0);

        // The type an uploader declares is served back as it arrived, so a browser must neither
        // sniff its way past it nor render markup inline on the app's own host.
        var markup = Blob.Text("<script>alert(1)</script>");
        var uploaded = await Uploads.UploadAsync(Server, rosa.Access, party.GeneralId, markup, "text/html", "page.html");
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);

        var served = await Uploads.DownloadAsync(Server, rosa.Access, uploaded.As(Attachment.Parser).Id);
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(markup, served.Body);
        Assert.Equal("text/html", served.Header("Content-Type"));
        Assert.Equal("nosniff", served.Header("X-Content-Type-Options"));
        Assert.Equal("attachment", served.Header("Content-Disposition"));
        Assert.Equal(["immutable", "max-age=31536000", "private"], served.Parts("Cache-Control"));
    }

    [Fact]
    public async Task A_stored_file_is_named_by_its_row_and_a_name_from_before_0_6_0_still_resolves()
    {
        await using var party = await Party.ConnectAsync(Server, "ulla");
        var (ulla, u1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;
        var store = Server.Services.GetRequiredService<AttachmentStore>();

        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(ulla, general, pixel);

        // The stored name depends on the row id alone, which is what lets an attachment be any
        // file at all; the overload that takes a type resolves to the same name.
        var current = store.PathFor(attachment.Id);
        Assert.Equal($"{attachment.Id}.{AttachmentsOptions.StoredExtension}", Path.GetFileName(current));
        Assert.Equal(current, store.PathFor(attachment.Id, attachment.ContentType));
        Assert.True(File.Exists(current));

        // Every row gets a path, including one whose content type is not a type at all: a row
        // whose file could not be named is a file nothing would ever delete.
        var paths = store.PathsFor([(attachment.Id, attachment.ContentType), (attachment.Id + 1, "junk"), (attachment.Id + 2, string.Empty)]);
        Assert.Equal(3, paths.Count);
        Assert.All(paths, path => Assert.EndsWith($".{AttachmentsOptions.StoredExtension}", path, StringComparison.Ordinal));

        // A row written before 0.6.0 has its bytes under <id>.<ext>, and neither the download nor
        // the delete may miss them.
        var legacy = Path.Combine(Server.AttachmentsDir, $"{attachment.Id}.png");
        File.Move(current, legacy);
        Assert.Equal(legacy, store.ExistingPathFor(attachment.Id, attachment.ContentType));

        var served = await Uploads.DownloadAsync(Server, ulla.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(pixel, served.Body);

        var caption = $"legacy {Names.Token()}";
        await u1.SendAsync(Frames.Send(caption, general, 0, attachment.Id));
        var messageId = await party.ExpectMessageEverywhereAsync(caption, general);
        await u1.SendAsync(Frames.Delete(messageId));
        await party.ExpectEverywhereAsync(Kind.MessageDeleted);

        // The broadcast goes out before the files do, so the frame alone does not say the delete
        // has finished; one connection's frames are handled in order, so the pong does.
        await u1.PingFenceAsync(Frames.NowMs());
        Assert.False(File.Exists(legacy));
        var gone = await Uploads.DownloadAsync(Server, ulla.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
    }

    [Fact]
    public async Task An_upload_is_complete_only_once_its_bytes_are_on_disk_and_only_a_complete_one_may_be_linked()
    {
        await using var party = await Party.ConnectAsync(Server, "vera");
        var (vera, v1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Data.AppDbContext>>();

        var body = Blob.Of(512);
        var stored = await Uploads.UploadAsync(Server, vera.Access, general, body, StreamApi.Octets, "whole.bin");
        Assert.Equal(HttpStatusCode.Created, stored.Status);
        var attachment = stored.As(Attachment.Parser);

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = await db.Attachments.AsNoTracking().SingleAsync(a => a.Id == attachment.Id);
            Assert.True(row.Complete, "a stored upload is not marked complete");
        }

        // A body that ends before its header promised is not an upload at all: no row, no file.
        var truncated = await Uploads.UploadAsync(
            Server,
            vera.Access,
            general,
            body,
            StreamApi.Octets,
            declaredLength: body.Length + 64);
        Assert.Equal(HttpStatusCode.BadRequest, truncated.Status);
        Assert.Equal("body ended early", truncated.Detail);

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var mine = await db.Attachments.AsNoTracking().CountAsync(a => a.UploaderId == vera.UserId);
            Assert.Equal(1, mine);
        }

        // A row whose bytes are still arriving exists before its file does, and a message may not
        // name one: the claim requires Complete.
        long unfinished;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = new Data.Attachment
            {
                ChannelId = general,
                UploaderId = vera.UserId,
                FileName = "in-flight.bin",
                ContentType = StreamApi.Octets,
                Size = 4096,
                Complete = false,
                CreatedAt = DateTime.UtcNow,
            };
            db.Attachments.Add(row);
            await db.SaveChangesAsync();
            unfinished = row.Id;
        }

        await v1.SendAsync(Frames.Send("too soon", general, 0, unfinished));
        await v1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = await db.Attachments.AsNoTracking().SingleAsync(a => a.Id == unfinished);
            Assert.Null(row.MessageId);
        }
    }

    // The whole grammar an attachment's declared type has to satisfy: an RFC 9110 type/subtype and
    // nothing else, because nothing on the upload path interprets it.
    [Theory]
    [InlineData("image/png")]
    [InlineData("application/vnd.ms-excel")]
    [InlineData("x-thing/y+z")]
    [InlineData("application/octet-stream")]
    [InlineData("a/b")]
    public void A_type_and_a_subtype_is_a_media_type(string value)
    {
        Assert.True(AttachmentsOptions.IsValidMediaType(value), value);
    }

    [Theory]
    [InlineData("")]
    [InlineData("image")]
    [InlineData("/png")]
    [InlineData("image/")]
    [InlineData("image/png; charset=utf-8")]
    [InlineData("image /png")]
    [InlineData("image/png/extra")]
    public void Anything_else_is_not_a_media_type(string value)
    {
        Assert.False(AttachmentsOptions.IsValidMediaType(value), value);
    }

    // The stored name comes from the row id, so a file name is metadata and never a path; it is
    // still echoed to every client, and the column holds 255 characters.
    [Theory]
    [InlineData(null, "file.bin")]
    [InlineData("", "file.bin")]
    [InlineData("   ", "file.bin")]
    [InlineData("\u0001\u0002", "file.bin")]
    [InlineData("holiday.mkv", "holiday.mkv")]
    [InlineData("../../etc/passwd", "passwd")]
    [InlineData("C:\\Users\\me\\notes.txt", "C:Usersmenotes.txt")]
    public void A_file_name_is_reduced_to_something_that_could_never_be_a_path(string? raw, string expected)
    {
        Assert.Equal(expected, AttachmentStore.SanitizeFileName(raw));
    }

    [Fact]
    public void A_file_name_is_cut_to_the_255_characters_its_column_holds()
    {
        Assert.Equal(new string('n', 255), AttachmentStore.SanitizeFileName(new string('n', 300)));
    }

    [Fact]
    public void A_media_type_of_129_characters_is_one_too_many()
    {
        Assert.True(AttachmentsOptions.IsValidMediaType(new string('a', 63) + "/" + new string('b', 64)));
        Assert.False(AttachmentsOptions.IsValidMediaType(new string('a', 64) + "/" + new string('b', 64)));
    }

    private async Task<Attachment> UploadPixelAsync(Account uploader, long channelId, byte[] pixel, string fileName = "pixel.png")
    {
        var response = await Uploads.UploadAsync(Server, uploader.Access, channelId, pixel, fileName: fileName);
        Assert.True(response.Status == HttpStatusCode.Created, $"upload to {channelId}: {(int)response.Status}");
        return response.As(Attachment.Parser);
    }
}
