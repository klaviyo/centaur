# Console authorization for code-registered workflows. Unconfigured workflows remain admin-only.
class WorkflowAccess < ApplicationRecord
  include WorkflowSharing

  belongs_to :owner, class_name: "User"
  validates :name, presence: true, uniqueness: true
  validates :name, exclusion: { in: [ ScheduledTask::WORKFLOW_NAME ] }

  scope :owned_by, ->(user) { where(owner: user) }

  def sharing_owner = owner
end
