require "test_helper"

module Api
  module V1
    class ConsoleSkillReadersTest < ActionDispatch::IntegrationTest
      setup do
        @owner = users(:acme_admin)
        @reader = users(:member_user)
        @editor = users(:globex_admin)
        @skill = skills(:other_private)
        @skill.readers << @reader
        @skill.editors << @editor
      end

      test "private reader sees instructions and mine listing but no management capabilities or identities" do
        sign_in_console(@reader)
        get api_v1_console_skill_path(@skill.oid)
        assert_response :ok
        assert_equal @skill.content, json_body.dig("data", "instructions")
        assert_empty json_body.dig("data", "readers")
        assert_equal({ "owner" => false, "can_edit" => false, "can_manage" => false, "can_unshare" => false },
                     json_body.dig("data", "viewer"))
        get api_v1_console_skills_path(tab: "mine")
        assert_includes json_body["data"].map { |s| s["id"] }, @skill.oid
        patch api_v1_console_skill_path(@skill.oid), params: { data: { instructions: "forbidden" } }, headers: csrf_headers, as: :json
        assert_response :not_found
        post share_api_v1_console_skill_path(@skill.oid), headers: csrf_headers
        assert_response :not_found
        delete api_v1_console_skill_path(@skill.oid), headers: csrf_headers
        assert_response :not_found
        assert_equal @skill.content, @skill.reload.content
      end

      test "only owner manages and sees readers while editors retain content edit access" do
        sign_in_console(@editor)
        patch api_v1_console_skill_path(@skill.oid), params: { data: { instructions: "Edited", reader_ids: [] } }, headers: csrf_headers, as: :json
        assert_response :ok
        assert json_body.dig("data", "viewer", "can_edit")
        assert_not json_body.dig("data", "viewer", "can_manage")
        assert_empty json_body.dig("data", "readers")
        assert_equal [ @reader.id ], @skill.reload.reader_ids
        sign_in_console(@owner)
        get api_v1_console_skill_path(@skill.oid)
        assert_equal [ @reader.oid ], json_body.dig("data", "readers").map { |r| r["id"] }
        patch api_v1_console_skill_path(@skill.oid), params: { data: { reader_ids: [] } }, headers: csrf_headers, as: :json
        assert_response :ok
        assert_empty @skill.reload.reader_ids
        sign_in_console(@reader)
        get api_v1_console_skill_path(@skill.oid)
        assert_response :not_found
      end

      test "invalid reader grants reject the entire content update" do
        sign_in_console(@owner)
        original = @skill.content
        patch api_v1_console_skill_path(@skill.oid),
              params: { data: { instructions: "must not save", reader_ids: [ users(:disabled_user).oid ] } },
              headers: csrf_headers, as: :json
        assert_response :unprocessable_entity
        assert_equal original, @skill.reload.content
        assert_equal [ @reader.id ], @skill.reader_ids
      end

      test "unrelated admins cannot inspect private reader membership" do
        @skill.editors.clear
        sign_in_console(@editor)
        get api_v1_console_skill_path(@skill.oid)
        assert_response :not_found
      end
    end
  end
end
