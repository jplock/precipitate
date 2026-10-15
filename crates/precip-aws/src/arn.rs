//! ARNs the scanner builds from a resource id, for resources whose
//! describe call returns no ARN. One table, so every collector that
//! refers to an instance or a subnet builds the same string.

use precip_types::{AccountId, Arn, Region};

/// A resource type whose ARN is `arn:<partition>:<service>:<region>:
/// <account>:<prefix><id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArnType {
    Instance,
    Volume,
    Image,
    ElasticIp,
    Vpc,
    Subnet,
    SecurityGroup,
    SecurityGroupRule,
    RouteTable,
    InternetGateway,
    NatGateway,
    VpcPeeringConnection,
    PrefixList,
    /// A launch template version; the id is `<template id>:<version>`.
    LaunchTemplate,
    /// Instance types belong to a region, not an account.
    InstanceType,
    LambdaFunction,
    DynamoDbTable,
    RdsInstance,
    EksCluster,
    EfsFileSystem,
}

impl ArnType {
    fn service(self) -> &'static str {
        match self {
            Self::Instance
            | Self::Volume
            | Self::Image
            | Self::ElasticIp
            | Self::Vpc
            | Self::Subnet
            | Self::SecurityGroup
            | Self::SecurityGroupRule
            | Self::RouteTable
            | Self::InternetGateway
            | Self::NatGateway
            | Self::VpcPeeringConnection
            | Self::PrefixList
            | Self::LaunchTemplate
            | Self::InstanceType => "ec2",
            Self::LambdaFunction => "lambda",
            Self::DynamoDbTable => "dynamodb",
            Self::RdsInstance => "rds",
            Self::EksCluster => "eks",
            Self::EfsFileSystem => "elasticfilesystem",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Instance => "instance/",
            Self::Volume => "volume/",
            Self::Image => "image/",
            Self::ElasticIp => "elastic-ip/",
            Self::Vpc => "vpc/",
            Self::Subnet => "subnet/",
            Self::SecurityGroup => "security-group/",
            Self::SecurityGroupRule => "security-group-rule/",
            Self::RouteTable => "route-table/",
            Self::InternetGateway => "internet-gateway/",
            Self::NatGateway => "natgateway/",
            Self::VpcPeeringConnection => "vpc-peering-connection/",
            Self::PrefixList => "prefix-list/",
            Self::LaunchTemplate => "launch-template/",
            Self::InstanceType => "instance-type/",
            Self::LambdaFunction => "function:",
            Self::DynamoDbTable => "table/",
            Self::RdsInstance => "db:",
            Self::EksCluster => "cluster/",
            Self::EfsFileSystem => "file-system/",
        }
    }

    /// The type whose CloudFormation physical id is the id in its ARN.
    pub(crate) fn from_cloudformation(resource_type: &str) -> Option<Self> {
        Some(match resource_type {
            "AWS::EC2::Instance" => Self::Instance,
            "AWS::EC2::Volume" => Self::Volume,
            "AWS::EC2::SecurityGroup" => Self::SecurityGroup,
            "AWS::EC2::VPC" => Self::Vpc,
            "AWS::EC2::Subnet" => Self::Subnet,
            "AWS::Lambda::Function" => Self::LambdaFunction,
            "AWS::DynamoDB::Table" => Self::DynamoDbTable,
            "AWS::RDS::DBInstance" => Self::RdsInstance,
            "AWS::EKS::Cluster" => Self::EksCluster,
            "AWS::EFS::FileSystem" => Self::EfsFileSystem,
            _ => return None,
        })
    }

    pub(crate) fn arn(
        self,
        partition: &str,
        region: &Region,
        account_id: &AccountId,
        id: &str,
    ) -> Arn {
        let account = match self {
            Self::InstanceType => "",
            Self::Instance
            | Self::Volume
            | Self::Image
            | Self::ElasticIp
            | Self::Vpc
            | Self::Subnet
            | Self::SecurityGroup
            | Self::SecurityGroupRule
            | Self::RouteTable
            | Self::InternetGateway
            | Self::NatGateway
            | Self::VpcPeeringConnection
            | Self::PrefixList
            | Self::LaunchTemplate
            | Self::LambdaFunction
            | Self::DynamoDbTable
            | Self::RdsInstance
            | Self::EksCluster
            | Self::EfsFileSystem => account_id.as_str(),
        };
        Arn::from(format!(
            "arn:{partition}:{}:{region}:{account}:{}{id}",
            self.service(),
            self.prefix()
        ))
    }
}

/// The partition segment of an ARN (`arn:<partition>:...`).
pub(crate) fn partition_of(arn: &str) -> Option<&str> {
    let mut parts = arn.splitn(3, ':');
    match (parts.next(), parts.next()) {
        (Some("arn"), Some(p)) if !p.is_empty() => Some(p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arn(ty: ArnType, partition: &str, id: &str) -> String {
        ty.arn(
            partition,
            &Region::from("cn-north-1"),
            &AccountId::from("1"),
            id,
        )
        .to_string()
    }

    #[test]
    fn arns_use_the_given_partition_service_and_prefix() {
        assert_eq!(
            arn(ArnType::Instance, "aws-cn", "i-1"),
            "arn:aws-cn:ec2:cn-north-1:1:instance/i-1"
        );
        assert_eq!(
            arn(ArnType::LambdaFunction, "aws", "f"),
            "arn:aws:lambda:cn-north-1:1:function:f"
        );
        assert_eq!(
            arn(ArnType::EfsFileSystem, "aws", "fs-1"),
            "arn:aws:elasticfilesystem:cn-north-1:1:file-system/fs-1"
        );
        assert_eq!(
            arn(ArnType::InstanceType, "aws", "m5.large"),
            "arn:aws:ec2:cn-north-1::instance-type/m5.large",
            "instance types have no account"
        );
    }

    #[test]
    fn cloudformation_types_map_to_id_based_arns_only() {
        assert_eq!(
            ArnType::from_cloudformation("AWS::EC2::Instance"),
            Some(ArnType::Instance)
        );
        assert_eq!(ArnType::from_cloudformation("AWS::EC2::EIP"), None);
        assert_eq!(ArnType::from_cloudformation("AWS::S3::Bucket"), None);
    }

    #[test]
    fn partition_comes_from_the_second_segment() {
        assert_eq!(
            partition_of("arn:aws-us-gov:iam::1:user/x"),
            Some("aws-us-gov")
        );
        assert_eq!(partition_of("arn::iam::1:user/x"), None);
        assert_eq!(partition_of("not-an-arn"), None);
    }
}
